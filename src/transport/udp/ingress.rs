// author: kodeholic (powered by Claude)
//! Ingress — publish RTP/RTCP 수신 처리 (hot path)
//!
//! - handle_srtp: publish RTP decrypt → RTP cache → egress 큐 fan-out
//! - handle_subscribe_rtcp: subscribe RTCP → NACK/RR/PLI/REMB 분기
//! - handle_nack_block: NACK → cache 조회 → RTX 조립 → egress 큐
//! - relay_publish_rtcp: publish RTCP(SR) → egress 큐 fan-out
//! - handle_mbcp_from_publish: MBCP APP → Floor Control 처리 (Phase M-1)

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tracing::{debug, info, trace, warn};

use crate::config;
use crate::config::RoomMode;
use crate::room::participant::{PcType, EgressPacket, TrackKind};
use crate::room::floor::FloorAction;
use crate::room::room::Room;

use super::UdpTransport;
use super::rtcp::{
    parse_rtp_header, current_ts, split_compound_rtcp, parse_rtcp_nack,
    expand_nack, build_rtx_packet, assemble_compound, build_mbcp_app, MbcpMessage,
};
use super::twcc;

impl UdpTransport {
    /// SRTP hot path — publish RTP decrypt → fan-out to subscriber egress queues
    pub(crate) async fn handle_srtp(&self, buf: &[u8], remote: std::net::SocketAddr) {
        let seq_num = self.dbg_rtp_count.fetch_add(1, Ordering::Relaxed);

        // O(1) lookup: addr → (participant, pc_type, room)
        let (sender, pc_type, room) = match self.room_hub.find_by_addr(&remote) {
            Some(r) => r,
            None => {
                if seq_num < config::DBG_DETAIL_LIMIT {
                    debug!("[DBG:RTP] from unknown addr={} srtp_len={}", remote, buf.len());
                }
                return;
            }
        };

        sender.touch(current_ts());

        // Subscribe PC에서 오는 패킷: RTCP feedback (NACK 등)
        if pc_type != PcType::Publish {
            self.handle_subscribe_rtcp(buf, remote, &sender, &room, seq_num).await;
            return;
        }

        if !sender.publish.is_media_ready() {
            if seq_num < config::DBG_DETAIL_LIMIT {
                debug!("[DBG:RTP] before DTLS complete user={} addr={}", sender.user_id, remote);
            }
            return;
        }

        // Detect RTCP vs RTP (RFC 5761 demux: PT 72-79 = RTCP)
        let is_rtcp = buf.get(1)
            .map(|b| { let pt = b & 0x7F; (72..=79).contains(&pt) })
            .unwrap_or(false);

        if is_rtcp {
            let is_detail = seq_num < config::DBG_DETAIL_LIMIT;

            // Decrypt SRTCP (publish session inbound)
            let plaintext = {
                let lock_t = Instant::now();
                let mut ctx = sender.publish.inbound_srtp.lock().unwrap();
                self.metrics.lock_wait.record(lock_t.elapsed().as_micros() as u64);
                let dec_t = Instant::now();
                match ctx.decrypt_rtcp(buf) {
                    Ok(p) => {
                        self.metrics.decrypt.record(dec_t.elapsed().as_micros() as u64);
                        p
                    }
                    Err(e) => {
                        self.metrics.decrypt_fail.fetch_add(1, Ordering::Relaxed);
                        if is_detail {
                            debug!("[DBG:RTCP:PUB] decrypt FAILED user={}: {e}", sender.user_id);
                        }
                        return;
                    }
                }
            };

            // Phase M-1: Compound RTCP 파싱 — MBCP APP 블록 체크
            let parsed = split_compound_rtcp(&plaintext);
            if !parsed.mbcp_blocks.is_empty() {
                self.handle_mbcp_from_publish(&parsed.mbcp_blocks, &sender, &room, is_detail).await;
            }

            // Phase C-2a: SR relay — MBCP APP 블록을 제외한 나머지만 릴레이
            // MBCP는 이미 위에서 처리 + broadcast_mbcp_to_subscribers로 전달하므로,
            // relay에서 중복 전달하면 안 됨
            if parsed.mbcp_blocks.is_empty() {
                // MBCP 없으면 기존처럼 통째로 (불필요한 재조립 회피)
                self.relay_publish_rtcp(&plaintext, &sender, &room, is_detail);
            } else if !parsed.relay_blocks.is_empty() {
                // MBCP 있으면 relay 대상 블록만 재조립해서 전달
                let relay_slices: Vec<&[u8]> = parsed.relay_blocks.iter()
                    .map(|b| &plaintext[b.offset..b.offset + b.length])
                    .collect();
                let stripped = assemble_compound(&relay_slices);
                self.relay_publish_rtcp(&stripped, &sender, &room, is_detail);
            }
            // MBCP만 있고 relay 대상 없으면 릴레이 스킵
            return;
        }

        // Decrypt SRTP → plaintext RTP (from publish session)
        let plaintext = {
            let lock_t = Instant::now();
            let mut ctx = sender.publish.inbound_srtp.lock().unwrap();
            self.metrics.lock_wait.record(lock_t.elapsed().as_micros() as u64);
            let dec_t = Instant::now();
            match ctx.decrypt_rtp(buf) {
                Ok(p) => {
                    self.metrics.decrypt.record(dec_t.elapsed().as_micros() as u64);
                    p
                }
                Err(e) => {
                    self.metrics.decrypt_fail.fetch_add(1, Ordering::Relaxed);
                    if seq_num < config::DBG_DETAIL_LIMIT {
                        debug!("[DBG:RTP] decrypt FAILED user={} addr={} srtp_len={}: {e}",
                            sender.user_id, remote, buf.len());
                    }
                    return;
                }
            }
        };

        // Relay counter: publisher → 서버 RTP 수신 성공
        self.metrics.ingress_rtp_received.fetch_add(1, Ordering::Relaxed);

        // [DBG:RTP] Parse RTP header for logging
        let rtp_hdr = parse_rtp_header(&plaintext);
        let is_detail = seq_num < config::DBG_DETAIL_LIMIT;
        let is_summary = seq_num > 0 && seq_num % config::DBG_SUMMARY_INTERVAL == 0;

        // 비디오 RTP 캐시 (NACK → RTX 재전송용, audio는 skip)
        // PT 96 = VP8 (server_codec_policy)
        if rtp_hdr.pt == 96 {
            match sender.rtp_cache.lock() {
                Ok(mut cache) => {
                    cache.store(rtp_hdr.seq, &plaintext);
                    self.metrics.rtp_cache_stored.fetch_add(1, Ordering::Relaxed);
                }
                Err(_) => {
                    self.metrics.rtp_cache_lock_fail.fetch_add(1, Ordering::Relaxed);
                }
            }
        }

        // TWCC: transport-wide seq# 추출 + 도착 시간 기록
        if let Some(twcc_seq) = twcc::parse_twcc_seq(&plaintext, config::TWCC_EXTMAP_ID) {
            if let Ok(mut rec) = sender.twcc_recorder.lock() {
                rec.record(twcc_seq, Instant::now());
                self.metrics.twcc_recorded.fetch_add(1, Ordering::Relaxed);
            }
        }

        if is_detail {
            debug!("[DBG:RTP] #{} user={} ssrc=0x{:08X} pt={} seq={} ts={} marker={} payload_len={}",
                seq_num, sender.user_id,
                rtp_hdr.ssrc, rtp_hdr.pt, rtp_hdr.seq, rtp_hdr.timestamp,
                rtp_hdr.marker, plaintext.len().saturating_sub(rtp_hdr.header_len));
        } else if is_summary {
            trace!("[DBG:RTP] summary #{} user={} last_ssrc=0x{:08X} last_pt={} last_seq={}",
                seq_num, sender.user_id,
                rtp_hdr.ssrc, rtp_hdr.pt, rtp_hdr.seq);
        }

        // Phase E-1: PTT 모드 미디어 게이팅 — floor holder만 통과
        if room.mode == RoomMode::Ptt {
            let allowed = match room.floor.current_speaker() {
                Some(ref speaker) if speaker == &sender.user_id => true,
                _ => false,
            };
            if !allowed {
                self.metrics.ptt_rtp_gated.fetch_add(1, Ordering::Relaxed);
                if is_detail {
                    trace!("[DBG:PTT] RTP dropped user={} (not floor holder)", sender.user_id);
                }
                return;
            }
        }

        // Phase E-2/E-4: PTT 모드 SSRC 리라이팅 (오디오 + 비디오)
        let fanout_payload = if room.mode == RoomMode::Ptt {
            use crate::room::ptt_rewriter::{RewriteResult, is_vp8_keyframe};
            let mut rewritten = plaintext.clone();
            let result = if rtp_hdr.pt == 111 {
                // Audio (Opus) — 키프레임 대기 없음
                room.audio_rewriter.rewrite(&mut rewritten, &sender.user_id, false)
            } else if rtp_hdr.pt == 96 {
                // Video (VP8) — 키프레임 감지 후 리라이팅
                let keyframe = is_vp8_keyframe(&plaintext);
                if keyframe {
                    self.metrics.ptt_keyframe_arrived.fetch_add(1, Ordering::Relaxed);
                }

                room.video_rewriter.rewrite(&mut rewritten, &sender.user_id, keyframe)
            } else {
                RewriteResult::Skip
            };
            match result {
                RewriteResult::Ok => {
                    self.metrics.ptt_rtp_rewritten.fetch_add(1, Ordering::Relaxed);
                    if rtp_hdr.pt == 111 {
                        self.metrics.ptt_audio_rewritten.fetch_add(1, Ordering::Relaxed);
                    } else if rtp_hdr.pt == 96 {
                        self.metrics.ptt_video_rewritten.fetch_add(1, Ordering::Relaxed);
                    }
                    rewritten
                }
                RewriteResult::PendingKeyframe => {
                    self.metrics.ptt_video_pending_drop.fetch_add(1, Ordering::Relaxed);
                    if is_detail {
                        trace!("[DBG:PTT] video dropped (pending keyframe) user={} seq={}",
                            sender.user_id, rtp_hdr.seq);
                    }
                    return; // 키프레임 대기 중 — P-frame 드롭
                }
                RewriteResult::Skip => {
                    if rtp_hdr.pt == 96 {
                        self.metrics.ptt_video_skip.fetch_add(1, Ordering::Relaxed);
                    }
                    plaintext.clone()
                }
            }
        } else {
            plaintext.clone()
        };

        // Phase W-3: egress 큐로 전달 (DashMap iter 직접 순회, Vec 할당 없음)
        if is_detail {
            let target_info: Vec<String> = room.participants.iter()
                .filter(|e| e.key() != &sender.user_id && e.value().is_subscribe_ready())
                .map(|e| format!("{}@{}", e.value().user_id, e.value().subscribe.get_address()
                    .map(|a| a.to_string()).unwrap_or("none".into())))
                .collect();
            debug!("[DBG:RELAY] #{} from={} targets=[{}]",
                seq_num, sender.user_id, target_info.join(", "));
        }

        for entry in room.participants.iter() {
            if entry.key() == &sender.user_id { continue; }
            let target = entry.value();
            if !target.is_subscribe_ready() { continue; }
            if target.egress_tx.try_send(EgressPacket::Rtp(fanout_payload.clone())).is_err() {
                self.metrics.egress_drop.fetch_add(1, Ordering::Relaxed);
            } else {
                self.metrics.egress_rtp_relayed.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    // ========================================================================
    // Phase M-1: MBCP Floor Control via RTCP APP (publish PC에서 수신)
    // ========================================================================

    /// Publish RTCP compound 내 MBCP APP 블록 처리
    ///
    /// 클라이언트가 SRTP 채널로 RTCP APP 패킷을 보내면 여기서 수신.
    /// SSRC로 참가자를 이미 식별한 상태이므로, user_id를 바로 사용.
    /// Floor 로직은 기존 floor.rs 상태 머신을 그대로 재사용.
    async fn handle_mbcp_from_publish(
        &self,
        mbcp_blocks: &[MbcpMessage],
        sender: &Arc<crate::room::participant::Participant>,
        room: &Arc<Room>,
        is_detail: bool,
    ) {
        if room.mode != RoomMode::Ptt {
            if is_detail {
                debug!("[MBCP] ignored — room {} is not PTT mode", room.id);
            }
            return;
        }

        let user_id = &sender.user_id;
        let now = current_ts();

        for msg in mbcp_blocks {
            match msg.subtype {
                config::MBCP_SUBTYPE_FREQ => {
                    info!("[MBCP] FLOOR_REQUEST user={} ssrc=0x{:08X}", user_id, msg.ssrc);
                    let action = room.floor.request(user_id, now);
                    self.apply_mbcp_floor_action(&action, room, sender, is_detail).await;

                    // Granted → rewriter 전환 + PLI (기존 WS 핸들러와 동일 로직)
                    if let FloorAction::Granted { ref speaker } = action {
                        self.metrics.ptt_floor_granted.fetch_add(1, Ordering::Relaxed);
                        self.metrics.ptt_speaker_switches.fetch_add(1, Ordering::Relaxed);
                        room.audio_rewriter.switch_speaker(speaker);
                        room.video_rewriter.switch_speaker(speaker);
                        self.send_pli_burst(room, speaker).await;
                    }
                }
                config::MBCP_SUBTYPE_FREL => {
                    info!("[MBCP] FLOOR_RELEASE user={} ssrc=0x{:08X}", user_id, msg.ssrc);
                    let action = room.floor.release(user_id);

                    if matches!(&action, FloorAction::Released { .. }) {
                        self.metrics.ptt_floor_released.fetch_add(1, Ordering::Relaxed);
                        room.audio_rewriter.clear_speaker();
                        room.video_rewriter.clear_speaker();
                    }

                    self.apply_mbcp_floor_action(&action, room, sender, is_detail).await;
                }
                config::MBCP_SUBTYPE_FPNG => {
                    let action = room.floor.ping(user_id, now);
                    if is_detail {
                        debug!("[MBCP] FLOOR_PING user={} result={:?}", user_id, action);
                    }
                    // PING은 응답을 보내지 않음 (UDP이므로 단방향 heartbeat)
                    // 서버는 last_ping 갱신만 하면 됨
                }
                _ => {
                    // 클라이언트가 FTKN/FIDL/FRVK를 보내는 것은 프로토콜 위반
                    warn!("[MBCP] unexpected subtype={} from user={}", msg.subtype, user_id);
                }
            }
        }
    }

    /// FloorAction → MBCP APP 패킷 브로드캐스트 (서버 → 모든 subscriber)
    ///
    /// WS 시그널링의 apply_floor_action과 동일한 의미론이지만,
    /// 메시지를 RTCP APP 패킷으로 조립해서 subscriber egress 큐에 넣는다.
    async fn apply_mbcp_floor_action(
        &self,
        action: &FloorAction,
        room: &Arc<Room>,
        requester: &Arc<crate::room::participant::Participant>,
        is_detail: bool,
    ) {
        match action {
            FloorAction::Granted { speaker } => {
                // 전체에 FTKN 브로드캐스트 (요청자 포함)
                let ftkn = build_mbcp_app(
                    config::MBCP_SUBTYPE_FTKN,
                    0, // 서버 SSRC = 0
                    Some(speaker),
                );
                self.broadcast_mbcp_to_subscribers(room, &ftkn);

                // 요청자에게도 WS FLOOR_TAKEN 전송 (하이브리드: WS 클라이언트 호환)
                self.send_ws_floor_taken(room, speaker);

                if is_detail {
                    debug!("[MBCP] FTKN broadcast speaker={}", speaker);
                }
            }
            FloorAction::Denied { reason, current_speaker } => {
                // 요청자에게만 거부 응답 — FRVK subtype 재사용 (cause에 이유)
                // 네이티브 클라이언트는 FRVK 수신 시 subtype로 구분 가능
                let deny_msg = format!("denied: {} (speaker={})", reason, current_speaker);
                let frvk = build_mbcp_app(
                    config::MBCP_SUBTYPE_FRVK,
                    0,
                    Some(&deny_msg),
                );
                self.send_mbcp_to_participant(requester, &frvk);

                if is_detail {
                    debug!("[MBCP] denied user={} reason={}", requester.user_id, reason);
                }
            }
            FloorAction::Released { prev_speaker } => {
                // 전체에 FIDL 브로드캐스트
                let fidl = build_mbcp_app(
                    config::MBCP_SUBTYPE_FIDL,
                    0,
                    Some(prev_speaker),
                );
                self.broadcast_mbcp_to_subscribers(room, &fidl);

                // WS 호환 이벤트
                self.send_ws_floor_idle(room, prev_speaker);

                if is_detail {
                    debug!("[MBCP] FIDL broadcast prev_speaker={}", prev_speaker);
                }
            }
            FloorAction::Revoked { prev_speaker, cause } => {
                // prev_speaker에게 FRVK
                if let Some(p) = room.get_participant(prev_speaker) {
                    let frvk = build_mbcp_app(
                        config::MBCP_SUBTYPE_FRVK,
                        0,
                        Some(cause),
                    );
                    self.send_mbcp_to_participant(&p, &frvk);
                }
                // 전체에 FIDL
                let fidl = build_mbcp_app(
                    config::MBCP_SUBTYPE_FIDL,
                    0,
                    Some(prev_speaker),
                );
                self.broadcast_mbcp_to_subscribers(room, &fidl);

                // WS 호환 이벤트
                self.send_ws_floor_idle(room, prev_speaker);

                if is_detail {
                    debug!("[MBCP] FRVK → {} cause={}, FIDL broadcast", prev_speaker, cause);
                }
            }
            _ => {} // PingOk, PingDenied — MBCP에서는 무응답
        }
    }

    /// MBCP APP 패킷을 room 내 모든 subscriber egress 큐에 전달
    fn broadcast_mbcp_to_subscribers(&self, room: &Arc<Room>, mbcp_pkt: &[u8]) {
        let pkt = mbcp_pkt.to_vec();
        for entry in room.participants.iter() {
            let target = entry.value();
            if !target.is_subscribe_ready() { continue; }
            if target.egress_tx.try_send(EgressPacket::Rtcp(pkt.clone())).is_err() {
                self.metrics.egress_drop.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// MBCP APP 패킷을 특정 participant의 subscriber egress 큐에 전달
    fn send_mbcp_to_participant(
        &self,
        participant: &Arc<crate::room::participant::Participant>,
        mbcp_pkt: &[u8],
    ) {
        if !participant.is_subscribe_ready() { return; }
        let pkt = mbcp_pkt.to_vec();
        if participant.egress_tx.try_send(EgressPacket::Rtcp(pkt)).is_err() {
            self.metrics.egress_drop.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// WS 호환: Floor Taken 이벤트를 WS로도 전송 (웹 클라이언트용)
    fn send_ws_floor_taken(&self, room: &Arc<Room>, speaker: &str) {
        let json = serde_json::json!({
            "op": crate::signaling::opcode::FLOOR_TAKEN,
            "pid": 0,
            "d": { "room_id": room.id, "speaker": speaker },
        });
        let msg = serde_json::to_string(&json).unwrap_or_default();
        for entry in room.participants.iter() {
            let _ = entry.value().ws_tx.send(msg.clone());
        }
    }

    /// WS 호환: Floor Idle 이벤트를 WS로도 전송 (웹 클라이언트용)
    fn send_ws_floor_idle(&self, room: &Arc<Room>, prev_speaker: &str) {
        let json = serde_json::json!({
            "op": crate::signaling::opcode::FLOOR_IDLE,
            "pid": 0,
            "d": { "room_id": room.id, "prev_speaker": prev_speaker },
        });
        let msg = serde_json::to_string(&json).unwrap_or_default();
        for entry in room.participants.iter() {
            let _ = entry.value().ws_tx.send(msg.clone());
        }
    }

    /// Floor Granted 시 PLI burst 전송 (0ms + 500ms + 1500ms)
    /// 기존 WS 핸들러의 PLI 3연발 로직과 동일
    async fn send_pli_burst(&self, room: &Arc<Room>, speaker: &str) {
        let participant = match room.get_participant(speaker) {
            Some(p) => p,
            None => return,
        };

        if !participant.is_publish_ready() { return; }

        let video_ssrc = {
            let tracks = participant.tracks.lock().unwrap();
            tracks.iter()
                .find(|t| t.kind == TrackKind::Video)
                .map(|t| t.ssrc)
        };

        let (ssrc, pub_addr) = match (video_ssrc, participant.publish.get_address()) {
            (Some(s), Some(a)) => (s, a),
            _ => return,
        };

        let p = Arc::clone(&participant);
        let socket = self.socket.clone();
        let speaker_id = speaker.to_string();

        // 이전 PLI burst가 진행 중이면 cancel
        participant.cancel_pli_burst();

        let handle = tokio::spawn(async move {
            let delays = [0u64, 500, 1500];
            for (i, &delay_ms) in delays.iter().enumerate() {
                if delay_ms > 0 {
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                }
                let pli_plain = super::rtcp::build_pli(ssrc);
                let encrypted = {
                    let mut ctx = p.publish.outbound_srtp.lock().unwrap();
                    ctx.encrypt_rtcp(&pli_plain).ok()
                };
                if let Some(enc) = encrypted {
                    if let Err(e) = socket.send_to(&enc, pub_addr).await {
                        warn!("[MBCP] PLI send FAILED user={} ssrc=0x{:08X} #{}: {e}",
                            speaker_id, ssrc, i);
                        break;
                    } else {
                        info!("[MBCP] PLI sent user={} ssrc=0x{:08X} #{} (floor granted)",
                            speaker_id, ssrc, i);
                    }
                }
            }
        });

        // AbortHandle 저장 (참가자 퇴장 시 cancel 가능하도록)
        *participant.pli_burst_handle.lock().unwrap() = Some(handle.abort_handle());
    }

    // ========================================================================
    // Subscribe RTCP — compound 파싱 → NACK 서버 처리 + 나머지 publisher 릴레이
    // ========================================================================

    /// Subscribe PC에서 수신된 RTCP 처리
    /// - NACK (PT=205): 서버에서 RTX 재전송 (기존 Phase C 로직)
    /// - RR/PLI/REMB: 해당 publisher의 publish PC로 transparent relay
    async fn handle_subscribe_rtcp(
        &self,
        buf: &[u8],
        remote: std::net::SocketAddr,
        subscriber: &Arc<crate::room::participant::Participant>,
        room: &Arc<Room>,
        seq_num: u64,
    ) {
        let is_detail = seq_num < config::DBG_DETAIL_LIMIT;
        self.metrics.sub_rtcp_received.fetch_add(1, Ordering::Relaxed);

        // RTCP인지 확인 (RFC 5761: PT 72-79)
        let is_rtcp = buf.get(1)
            .map(|b| { let pt = b & 0x7F; (72..=79).contains(&pt) })
            .unwrap_or(false);

        if !is_rtcp {
            self.metrics.sub_rtcp_not_rtcp.fetch_add(1, Ordering::Relaxed);
            if is_detail {
                trace!("[DBG:SUB] non-RTCP from subscribe PC user={} addr={} byte0=0x{:02X} byte1=0x{:02X}",
                    subscriber.user_id, remote,
                    buf.get(0).copied().unwrap_or(0),
                    buf.get(1).copied().unwrap_or(0));
            }
            return;
        }

        // Subscribe session의 inbound_srtp로 decrypt
        let plaintext = {
            let mut ctx = subscriber.subscribe.inbound_srtp.lock().unwrap();
            match ctx.decrypt_rtcp(buf) {
                Ok(p) => {
                    self.metrics.sub_rtcp_decrypted.fetch_add(1, Ordering::Relaxed);
                    p
                }
                Err(e) => {
                    self.metrics.decrypt_fail.fetch_add(1, Ordering::Relaxed);
                    if is_detail {
                        debug!("[DBG:RTCP:SUB] SRTCP decrypt FAILED user={} addr={}: {e}",
                            subscriber.user_id, remote);
                    }
                    return;
                }
            }
        };

        // Compound RTCP 파싱: NACK 분리 + publisher별 릴레이 대상 수집
        let parsed = split_compound_rtcp(&plaintext);

        if is_detail {
            debug!("[DBG:RTCP:SUB] user={} compound_len={} nack_blocks={} relay_blocks={} mbcp_blocks={}",
                subscriber.user_id, plaintext.len(), parsed.nack_blocks.len(),
                parsed.relay_blocks.len(), parsed.mbcp_blocks.len());
        }

        // (1) NACK 처리 (RTX 재전송 — 기존 로직)
        self.metrics.nack_received.fetch_add(parsed.nack_blocks.len() as u64, Ordering::Relaxed);
        for nack_block in &parsed.nack_blocks {
            self.handle_nack_block(nack_block, subscriber, room, is_detail);
        }

        // (2) 릴레이 대상 RTCP (RR, PLI, REMB) → publisher별로 모아서 전송
        if !parsed.relay_blocks.is_empty() {
            self.relay_subscribe_rtcp_blocks(&plaintext, &parsed, subscriber, room, is_detail).await;
        }

        // (3) MBCP APP 블록 — subscribe PC에서도 MBCP를 보낼 수 있음 (드문 경우)
        if !parsed.mbcp_blocks.is_empty() {
            self.handle_mbcp_from_publish(&parsed.mbcp_blocks, subscriber, room, is_detail).await;
        }
    }

    /// Subscribe RTCP의 릴레이 대상 블록을 publisher별로 모아서 전송
    async fn relay_subscribe_rtcp_blocks(
        &self,
        plaintext: &[u8],
        parsed: &super::rtcp::CompoundRtcpParsed,
        _subscriber: &Arc<crate::room::participant::Participant>,
        room: &Arc<Room>,
        is_detail: bool,
    ) {
        // RR/PLI relay count
        for block in &parsed.relay_blocks {
            let pt = plaintext.get(block.offset + 1).copied().unwrap_or(0);
            if pt == config::RTCP_PT_RR { self.metrics.rr_relayed.fetch_add(1, Ordering::Relaxed); }
            if pt == config::RTCP_PT_PSFB { self.metrics.pli_sent.fetch_add(1, Ordering::Relaxed); }
        }

        // Phase E-4: PTT 모드에서 가상 SSRC → 원본 SSRC 변환
        let ptt_audio_vssrc = if room.mode == RoomMode::Ptt {
            Some(room.audio_rewriter.virtual_ssrc())
        } else { None };
        let ptt_video_vssrc = if room.mode == RoomMode::Ptt {
            Some(room.video_rewriter.virtual_ssrc())
        } else { None };

        // media_ssrc → publisher 매핑 + RTCP 블록 그룹핑
        let mut publisher_rtcp: HashMap<u32, Vec<&[u8]>> = HashMap::new();
        for block in &parsed.relay_blocks {
            if block.media_ssrc == 0 { continue; }
            let effective_ssrc = if ptt_audio_vssrc == Some(block.media_ssrc)
                || ptt_video_vssrc == Some(block.media_ssrc) {
                let is_video = ptt_video_vssrc == Some(block.media_ssrc);
                room.floor.current_speaker()
                    .and_then(|uid| room.get_participant(&uid))
                    .and_then(|p| {
                        let tracks = p.tracks.lock().unwrap();
                        tracks.iter()
                            .find(|t| if is_video {
                                t.kind == TrackKind::Video
                            } else {
                                t.kind == TrackKind::Audio
                            })
                            .map(|t| t.ssrc)
                    })
                    .unwrap_or(block.media_ssrc)
            } else {
                block.media_ssrc
            };
            publisher_rtcp.entry(effective_ssrc)
                .or_default()
                .push(&plaintext[block.offset..block.offset + block.length]);
        }

        for (media_ssrc, blocks) in &publisher_rtcp {
            let publisher = room.find_by_track_ssrc(*media_ssrc);

            let publisher = match publisher {
                Some(p) => p,
                None => {
                    if is_detail {
                        debug!("[DBG:RTCP:SUB] publisher not found for ssrc=0x{:08X}", media_ssrc);
                    }
                    continue;
                }
            };

            if !publisher.is_publish_ready() { continue; }

            let pub_addr = match publisher.publish.get_address() {
                Some(a) => a,
                None => continue,
            };

            let compound = assemble_compound(blocks);

            let encrypted = {
                let mut ctx = publisher.publish.outbound_srtp.lock().unwrap();
                match ctx.encrypt_rtcp(&compound) {
                    Ok(p) => p,
                    Err(e) => {
                        if is_detail {
                            debug!("[DBG:RTCP:SUB] relay encrypt FAILED ssrc=0x{:08X}: {e}", media_ssrc);
                        }
                        continue;
                    }
                }
            };

            if let Err(e) = self.socket.send_to(&encrypted, pub_addr).await {
                if is_detail {
                    debug!("[DBG:RTCP:SUB] relay send FAILED ssrc=0x{:08X} addr={}: {e}",
                        media_ssrc, pub_addr);
                }
            } else {
                if is_detail {
                    debug!("[DBG:RTCP:SUB] relayed {} block(s) ssrc=0x{:08X} → user={} addr={}",
                        blocks.len(), media_ssrc, publisher.user_id, pub_addr);
                }
            }
        }
    }

    // ========================================================================
    // NACK 처리 (RTX 재전송 — 기존 Phase C 로직 추출)
    // ========================================================================

    /// 단일 NACK RTCP 블록 처리: 캐시 조회 → RTX 조립 → 전송
    fn handle_nack_block(
        &self,
        nack_data: &[u8],
        subscriber: &Arc<crate::room::participant::Participant>,
        room: &Arc<Room>,
        is_detail: bool,
    ) {
        let nack_items = parse_rtcp_nack(nack_data);

        // Phase E-4: PTT 모드 NACK 역매핑
        let is_ptt = room.mode == RoomMode::Ptt;
        let ptt_virtual_video_ssrc = if is_ptt {
            Some(room.video_rewriter.virtual_ssrc())
        } else {
            None
        };

        for nack in &nack_items {
            let lost_seqs = expand_nack(nack.pid, nack.blp);
            self.metrics.nack_seqs_requested.fetch_add(lost_seqs.len() as u64, Ordering::Relaxed);

            if is_detail {
                debug!("[DBG:NACK] user={} media_ssrc=0x{:08X} pid={} blp=0x{:04X} seqs={:?}",
                    subscriber.user_id, nack.media_ssrc, nack.pid, nack.blp, lost_seqs);
            }

            let (lookup_ssrc, cache_seqs) = if ptt_virtual_video_ssrc == Some(nack.media_ssrc) {
                self.metrics.ptt_nack_remapped.fetch_add(1, Ordering::Relaxed);
                let original_seqs: Vec<u16> = lost_seqs.iter()
                    .map(|&vs| room.video_rewriter.reverse_seq(vs))
                    .collect();
                let speaker_ssrc = room.floor.current_speaker()
                    .and_then(|uid| room.get_participant(&uid))
                    .and_then(|p| {
                        let tracks = p.tracks.lock().unwrap();
                        tracks.iter()
                            .find(|t| t.kind == TrackKind::Video)
                            .map(|t| t.ssrc)
                    })
                    .unwrap_or(nack.media_ssrc);
                (speaker_ssrc, original_seqs)
            } else {
                (nack.media_ssrc, lost_seqs.clone())
            };

            let publisher = room.find_by_track_ssrc(lookup_ssrc);

            let publisher = match publisher {
                Some(p) => p,
                None => {
                    self.metrics.nack_publisher_not_found.fetch_add(1, Ordering::Relaxed);
                    if is_detail {
                        debug!("[DBG:NACK] publisher not found for ssrc=0x{:08X}", lookup_ssrc);
                    }
                    continue;
                }
            };

            let rtx_ssrc = publisher.get_tracks().iter()
                .find(|t| t.ssrc == lookup_ssrc)
                .and_then(|t| t.rtx_ssrc);

            let rtx_ssrc = match rtx_ssrc {
                Some(s) => s,
                None => {
                    self.metrics.nack_no_rtx_ssrc.fetch_add(1, Ordering::Relaxed);
                    if is_detail {
                        debug!("[DBG:NACK] no rtx_ssrc for ssrc=0x{:08X}", lookup_ssrc);
                    }
                    continue;
                }
            };

            let rtx_packets: Vec<(u16, u16, Vec<u8>)> = {
                let cache = publisher.rtp_cache.lock().unwrap();
                cache_seqs.iter().filter_map(|&lost_seq| {
                    let original = cache.get(lost_seq)?;
                    let rtx_seq = publisher.next_rtx_seq();
                    let rtx_pkt = build_rtx_packet(original, rtx_ssrc, rtx_seq);
                    Some((lost_seq, rtx_seq, rtx_pkt))
                }).collect()
            };

            let cache_miss = cache_seqs.len() - rtx_packets.len();
            self.metrics.rtx_cache_miss.fetch_add(cache_miss as u64, Ordering::Relaxed);
            self.metrics.rtx_sent.fetch_add(rtx_packets.len() as u64, Ordering::Relaxed);

            if cache_miss > 0 {
                if self.metrics.rtx_cache_miss.load(Ordering::Relaxed) <= 10 {
                    let cache = publisher.rtp_cache.lock().unwrap();
                    let missed: Vec<String> = cache_seqs.iter()
                        .filter(|&&s| rtx_packets.iter().all(|(ls, _, _)| *ls != s))
                        .take(3)
                        .map(|&s| {
                            let idx = (s as usize) % crate::config::RTP_CACHE_SIZE;
                            let slot_info = match cache.slot_seq(s) {
                                None => "EMPTY".to_string(),
                                Some(cached) if cached == s => "MATCH(bug?)".to_string(),
                                Some(cached) => format!("OTHER({})", cached),
                            };
                            format!("seq={}(idx={})={}", s, idx, slot_info)
                        })
                        .collect();
                    debug!("[DBG:RTX] MISS {}/{} ssrc=0x{:08X} user={} cached_3s={} samples=[{}]",
                        cache_miss, cache_seqs.len(), lookup_ssrc,
                        publisher.user_id, self.metrics.rtp_cache_stored.load(Ordering::Relaxed), missed.join(", "));
                }
            }

            for (_lost_seq, _rtx_seq, rtx_pkt) in rtx_packets {
                if subscriber.egress_tx.try_send(EgressPacket::Rtp(rtx_pkt)).is_err() {
                    self.metrics.egress_drop.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }

    // ========================================================================
    // Publish RTCP relay — SR을 모든 subscriber에게 fan-out (Phase C-2a)
    // ========================================================================

    /// Publish PC에서 수신된 RTCP compound를 모든 subscriber에게 릴레이.
    /// SR 외 다른 RTCP도 함께 있을 수 있으나, compound 통째로 릴레이한다.
    /// (publish → subscribe 방향에는 NACK이 없으므로 분리 불필요)
    fn relay_publish_rtcp(
        &self,
        plaintext: &[u8],
        sender: &Arc<crate::room::participant::Participant>,
        room: &Arc<Room>,
        _is_detail: bool,
    ) {
        // Phase E-1: PTT 모드에서는 floor holder의 SR만 릴레이
        if room.mode == RoomMode::Ptt {
            let allowed = match room.floor.current_speaker() {
                Some(ref speaker) if speaker == &sender.user_id => true,
                _ => false,
            };
            if !allowed { return; }
        }

        self.metrics.sr_relayed.fetch_add(1, Ordering::Relaxed);

        // Phase W-3: egress 큐로 SR relay 전달 (DashMap iter 직접 순회, Vec 할당 없음)
        let plaintext_owned = plaintext.to_vec();

        for entry in room.participants.iter() {
            if entry.key() == &sender.user_id { continue; }
            let target = entry.value();
            if !target.is_subscribe_ready() { continue; }
            if target.egress_tx.try_send(EgressPacket::Rtcp(plaintext_owned.clone())).is_err() {
                self.metrics.egress_drop.fetch_add(1, Ordering::Relaxed);
            } else {
                self.metrics.egress_rtcp_relayed.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}
