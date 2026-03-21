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
use std::time::Instant;
use tracing::{debug, info, trace, warn};

use crate::config;
use crate::config::RoomMode;
use crate::room::participant::{PcType, EgressPacket, TrackKind, SimulcastRewriter, SubscribeLayerEntry};
use crate::room::floor::FloorAction;
use crate::room::room::Room;

use super::UdpTransport;
use super::rtcp::{
    parse_rtp_header, RtpHeader, current_ts, split_compound_rtcp, parse_rtcp_nack,
    expand_nack, build_rtx_packet, assemble_compound, build_mbcp_app, MbcpMessage,
};
use super::rtcp_terminator::{self, RecvStats};
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
            self.process_publish_rtcp(buf, &sender, &room, seq_num).await;
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
        sender.pipeline.pub_rtp_in.fetch_add(1, Ordering::Relaxed);

        // [DBG:RTP] Parse RTP header for logging
        let rtp_hdr = parse_rtp_header(&plaintext);
        let is_detail = seq_num < config::DBG_DETAIL_LIMIT;
        let is_summary = seq_num > 0 && seq_num % config::DBG_SUMMARY_INTERVAL == 0;

        // Per-packet stats: jitter, recv_stats, cache, TWCC
        self.collect_rtp_stats(&plaintext, &rtp_hdr, &sender, is_detail);

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

        // PTT gating + SSRC rewriting → fanout payload
        let fanout_payload = match self.prepare_fanout_payload(&plaintext, &rtp_hdr, &sender, &room, is_detail) {
            Some(p) => p,
            None => return, // gated or pending keyframe
        };

        // Phase W-3: egress 큐로 전달
        if is_detail {
            let target_info: Vec<String> = room.participants.iter()
                .filter(|e| e.key() != &sender.user_id && e.value().is_subscribe_ready())
                .map(|e| format!("{}@{}", e.value().user_id, e.value().subscribe.get_address()
                    .map(|a| a.to_string()).unwrap_or("none".into())))
                .collect();
            debug!("[DBG:RELAY] #{} from={} targets=[{}]",
                seq_num, sender.user_id, target_info.join(", "));
        }

        // Simulcast video fan-out: 레이어 선택 + SimulcastRewriter
        if room.simulcast_enabled && rtp_hdr.pt == 96 {
            self.fanout_simulcast_video(&fanout_payload, &rtp_hdr, &sender, &room).await;
        } else {
            // Normal fan-out (audio, non-simulcast video, PTT)
            for entry in room.participants.iter() {
                if entry.key() == &sender.user_id { continue; }
                let target = entry.value();
                if !target.is_subscribe_ready() { continue; }
                if target.egress_tx.try_send(EgressPacket::Rtp(fanout_payload.clone())).is_err() {
                    self.metrics.egress_drop.fetch_add(1, Ordering::Relaxed);
                    target.pipeline.sub_rtp_dropped.fetch_add(1, Ordering::Relaxed);
                    crate::agg_logger::inc_with(
                        crate::agg_logger::agg_key(&["egress_queue_full", &room.id]),
                        format!("egress_queue_full user={}", target.user_id),
                        Some(&room.id),
                    );
                } else {
                    self.metrics.egress_rtp_relayed.fetch_add(1, Ordering::Relaxed);
                    target.pipeline.sub_rtp_relayed.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }

    // ========================================================================
    // Publish RTCP processing (extracted from handle_srtp)
    // ========================================================================

    /// Publish RTCP: decrypt → MBCP → SR consume → SR translation + relay
    async fn process_publish_rtcp(
        &self,
        buf: &[u8],
        sender: &Arc<crate::room::participant::Participant>,
        room: &Arc<Room>,
        seq_num: u64,
    ) {
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

        // RTCP Terminator: Publisher SR 소비 (RecvStats LSR/DLSR 갱신)
        for sr_ref in &parsed.sr_blocks {
            let sr_data = &plaintext[sr_ref.offset..sr_ref.offset + sr_ref.length];
            if let Some((ssrc, ntp_hi, ntp_lo)) = rtcp_terminator::parse_sr_ntp(sr_data) {
                let mut stats_map = sender.recv_stats.lock().unwrap();
                if let Some(stats) = stats_map.get_mut(&ssrc) {
                    stats.on_sr_received(ntp_hi, ntp_lo);
                }
                if is_detail {
                    debug!("[RTCP:TERM] consumed SR from user={} ssrc=0x{:08X}",
                        sender.user_id, ssrc);
                }
            }
        }

        // SR translation + PLI/REMB 릴레이
        // SR: subscriber별로 SSRC/RTP ts/counts 변환 (PTT: 가상 SSRC, Conference: counts만)
        // RR: 서버가 자체 생성 (릴레이 안 함)
        let sr_data: Vec<Vec<u8>> = parsed.sr_blocks.iter()
            .map(|sr_ref| plaintext[sr_ref.offset..sr_ref.offset + sr_ref.length].to_vec())
            .collect();
        let relay_data: Vec<Vec<u8>> = parsed.relay_blocks.iter()
            .map(|blk| plaintext[blk.offset..blk.offset + blk.length].to_vec())
            .collect();
        if !sr_data.is_empty() || !relay_data.is_empty() {
            self.relay_publish_rtcp_translated(
                &sr_data, &relay_data, &sender, &room, is_detail,
            );
        }
        // MBCP만 있고 SR/PLI 없으면 릴레이 스킵
    }

    // ========================================================================
    // Per-packet RTP stats collection (extracted from handle_srtp)
    // ========================================================================

    /// Audio jitter 감지, RTCP recv_stats 갱신, RTP cache, TWCC 기록
    fn collect_rtp_stats(
        &self,
        plaintext: &[u8],
        rtp_hdr: &RtpHeader,
        sender: &Arc<crate::room::participant::Participant>,
        _is_detail: bool,
    ) {
        // Audio inter-arrival jitter 감지 (40ms 초과 시 warn 로그)
        if rtp_hdr.pt == 111 {
            let now_us = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_micros() as u64;
            let prev_us = sender.last_audio_arrival_us.swap(now_us, Ordering::Relaxed);
            if prev_us > 0 {
                let gap_ms = (now_us.saturating_sub(prev_us)) / 1000;
                if gap_ms > 40 {
                    crate::agg_logger::inc_with(
                        crate::agg_logger::agg_key(&["audio_gap", &sender.room_id, &sender.user_id]),
                        format!("audio_gap user={} gap={}ms", sender.user_id, gap_ms),
                        Some(&sender.room_id),
                    );
                }
            }
        }

        // RTCP Terminator: 수신 통계 갱신 (서버가 peer로서 RR 생성용)
        // RTX(PT=97)는 재전송 패킷이므로 수신 통계에서 제외 — jitter 폭등 방지
        if rtp_hdr.pt != config::RTX_PAYLOAD_TYPE {
            let arrival_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            let clock_rate = if rtp_hdr.pt == 111 {
                config::CLOCK_RATE_AUDIO
            } else {
                config::CLOCK_RATE_VIDEO
            };
            let mut stats_map = sender.recv_stats.lock().unwrap();
            let stats = stats_map.entry(rtp_hdr.ssrc)
                .or_insert_with(|| RecvStats::new(rtp_hdr.ssrc, clock_rate));
            stats.update(rtp_hdr.seq, rtp_hdr.timestamp, arrival_ms);
        }

        // 비디오 RTP 캐시 (NACK → RTX 재전송용, audio는 skip)
        // PT 96 = VP8 (server_codec_policy)
        if rtp_hdr.pt == 96 {
            match sender.rtp_cache.lock() {
                Ok(mut cache) => {
                    cache.store(rtp_hdr.seq, plaintext);
                    self.metrics.rtp_cache_stored.fetch_add(1, Ordering::Relaxed);
                }
                Err(_) => {
                    self.metrics.rtp_cache_lock_fail.fetch_add(1, Ordering::Relaxed);
                }
            }
        }

        // TWCC: transport-wide seq# 추출 + 도착 시간 기록
        // client-offer 모드: Chrome이 할당한 extmap ID 사용 (0이면 서버 기본값 fallback)
        let twcc_id = {
            let cid = sender.twcc_extmap_id.load(Ordering::Relaxed);
            if cid != 0 { cid } else { config::TWCC_EXTMAP_ID }
        };
        if let Some(twcc_seq) = twcc::parse_twcc_seq(plaintext, twcc_id) {
            if let Ok(mut rec) = sender.twcc_recorder.lock() {
                rec.record(twcc_seq, Instant::now());
                self.metrics.twcc_recorded.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    // ========================================================================
    // PTT gating + SSRC rewriting (extracted from handle_srtp)
    // ========================================================================

    /// PTT 모드: floor gating + SSRC rewrite. Conference: passthrough.
    /// Returns None if gated or pending keyframe (caller should return).
    fn prepare_fanout_payload(
        &self,
        plaintext: &[u8],
        rtp_hdr: &RtpHeader,
        sender: &Arc<crate::room::participant::Participant>,
        room: &Arc<Room>,
        is_detail: bool,
    ) -> Option<Vec<u8>> {
        // Phase E-1: PTT 모드 미디어 게이팅 — floor holder만 통과
        if room.mode == RoomMode::Ptt {
            let allowed = match room.floor.current_speaker() {
                Some(ref speaker) if speaker == &sender.user_id => true,
                _ => false,
            };
            if !allowed {
                self.metrics.ptt_rtp_gated.fetch_add(1, Ordering::Relaxed);
                sender.pipeline.pub_rtp_gated.fetch_add(1, Ordering::Relaxed);
                if is_detail {
                    trace!("[DBG:PTT] RTP dropped user={} (not floor holder)", sender.user_id);
                }
                return None;
            }
        }

        // Phase E-2/E-4: PTT 모드 SSRC 리라이팅 (오디오 + 비디오)
        if room.mode == RoomMode::Ptt {
            use crate::room::ptt_rewriter::{RewriteResult, is_vp8_keyframe};
            let mut rewritten = plaintext.to_vec();
            let result = if rtp_hdr.pt == 111 {
                // Audio (Opus) — 키프레임 대기 없음
                room.audio_rewriter.rewrite(&mut rewritten, &sender.user_id, false)
            } else if rtp_hdr.pt == 96 {
                // Video (VP8) — 키프레임 감지 후 리라이팅
                let keyframe = is_vp8_keyframe(plaintext);
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
                    sender.pipeline.pub_rtp_rewritten.fetch_add(1, Ordering::Relaxed);
                    if rtp_hdr.pt == 111 {
                        self.metrics.ptt_audio_rewritten.fetch_add(1, Ordering::Relaxed);
                    } else if rtp_hdr.pt == 96 {
                        self.metrics.ptt_video_rewritten.fetch_add(1, Ordering::Relaxed);
                    }
                    Some(rewritten)
                }
                RewriteResult::PendingKeyframe => {
                    self.metrics.ptt_video_pending_drop.fetch_add(1, Ordering::Relaxed);
                    sender.pipeline.pub_video_pending.fetch_add(1, Ordering::Relaxed);
                    if is_detail {
                        trace!("[DBG:PTT] video dropped (pending keyframe) user={} seq={}",
                            sender.user_id, rtp_hdr.seq);
                    }
                    None // 키프레임 대기 중 — P-frame 드롭
                }
                RewriteResult::Skip => {
                    if rtp_hdr.pt == 96 {
                        self.metrics.ptt_video_skip.fetch_add(1, Ordering::Relaxed);
                    }
                    Some(plaintext.to_vec())
                }
            }
        } else {
            Some(plaintext.to_vec())
        }
    }

    // ========================================================================
    // Simulcast video fan-out (extracted from handle_srtp)
    // ========================================================================

    /// Simulcast video: subscriber별 레이어 선택 + SimulcastRewriter + PLI 교착 방지
    async fn fanout_simulcast_video(
        &self,
        fanout_payload: &[u8],
        rtp_hdr: &RtpHeader,
        sender: &Arc<crate::room::participant::Participant>,
        room: &Arc<Room>,
    ) {
        let sender_rid = {
            let tracks = sender.tracks.lock().unwrap();
            tracks.iter()
                .find(|t| t.ssrc == rtp_hdr.ssrc)
                .and_then(|t| t.rid.clone())
        };
        let sender_rid = match sender_rid {
            Some(r) => r,
            None => return, // simulcast track이 아님
        };

        let is_keyframe = crate::room::ptt_rewriter::is_vp8_keyframe(fanout_payload);
        let vssrc = sender.ensure_simulcast_video_ssrc();
        let mut pli_sent = false;

        for entry in room.participants.iter() {
            if entry.key() == &sender.user_id { continue; }
            let target = entry.value();
            if !target.is_subscribe_ready() { continue; }

            let (forwarded_buf, need_pli) = {
                let mut layers = target.subscribe_layers.lock().unwrap();
                let mut created = false;
                let sub = layers.entry(sender.user_id.clone())
                    .or_insert_with(|| {
                        created = true;
                        SubscribeLayerEntry {
                            rid: "h".to_string(),
                            rewriter: SimulcastRewriter::new(vssrc),
                        }
                    });

                if sub.rid == "pause" || sender_rid != sub.rid {
                    (None, created)
                } else {
                    let mut video_buf = fanout_payload.to_vec();
                    let ok = sub.rewriter.rewrite(&mut video_buf, is_keyframe);
                    // PLI 자가 치유: rewrite 실패(키프레임 대기) + 재시도 시간 경과
                    let retry_pli = !ok && sub.rewriter.needs_pli_retry();
                    if created || retry_pli {
                        sub.rewriter.mark_pli_sent();
                    }
                    (if ok { Some(video_buf) } else { None }, created || retry_pli)
                }
            };

            // PLI: 즉석 생성 또는 자가 치유 재시도 → publisher에게 PLI burst (fan-out 회당 1회)
            if need_pli && !pli_sent {
                pli_sent = true;
                let h_ssrc = {
                    let tracks = sender.tracks.lock().unwrap();
                    tracks.iter()
                        .find(|t| t.kind == TrackKind::Video && t.rid.as_deref() == Some("h"))
                        .map(|t| t.ssrc)
                };
                if let (Some(ssrc), Some(pub_addr)) = (h_ssrc, sender.publish.get_address()) {
                    if sender.is_publish_ready() {
                        super::pli::spawn_pli_burst(sender, ssrc, pub_addr, self.socket.clone(), &[0, 200, 500, 1500], "SIM:PLI");
                    }
                }
            }

            if let Some(buf) = forwarded_buf {
                if target.egress_tx.try_send(EgressPacket::Rtp(buf)).is_err() {
                    self.metrics.egress_drop.fetch_add(1, Ordering::Relaxed);
                    target.pipeline.sub_rtp_dropped.fetch_add(1, Ordering::Relaxed);
                } else {
                    self.metrics.egress_rtp_relayed.fetch_add(1, Ordering::Relaxed);
                    target.pipeline.sub_rtp_relayed.fetch_add(1, Ordering::Relaxed);
                }
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
                    // MBCP UDP: priority=0 고정 (우선순위 요청은 WS만 지원)
                    let actions = room.floor.request(user_id, config::FLOOR_DEFAULT_PRIORITY, now);
                    self.apply_mbcp_floor_actions(&actions, room, sender, is_detail).await;
                }
                config::MBCP_SUBTYPE_FREL => {
                    info!("[MBCP] FLOOR_RELEASE user={} ssrc=0x{:08X}", user_id, msg.ssrc);
                    let actions = room.floor.release(user_id);

                    for a in &actions {
                        if matches!(a, FloorAction::Released { .. }) {
                            self.metrics.ptt_floor_released.fetch_add(1, Ordering::Relaxed);
                            room.audio_rewriter.clear_speaker();
                            room.video_rewriter.clear_speaker();
                        }
                    }

                    self.apply_mbcp_floor_actions(&actions, room, sender, is_detail).await;
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

    /// Vec<FloorAction> → MBCP APP 패킷 브로드캐스트 (서버 → 모든 subscriber)
    ///
    /// WS 시그널링의 apply_floor_actions와 동일한 의미론이지만,
    /// 메시지를 RTCP APP 패킷으로 조립해서 subscriber egress 큐에 넣는다.
    async fn apply_mbcp_floor_actions(
        &self,
        actions: &[FloorAction],
        room: &Arc<Room>,
        requester: &Arc<crate::room::participant::Participant>,
        is_detail: bool,
    ) {
        for action in actions {
            match action {
                FloorAction::Granted { speaker, priority, .. } => {
                    self.metrics.ptt_floor_granted.fetch_add(1, Ordering::Relaxed);
                    self.metrics.ptt_speaker_switches.fetch_add(1, Ordering::Relaxed);
                    room.audio_rewriter.switch_speaker(speaker);
                    room.video_rewriter.switch_speaker(speaker);
                    self.send_pli_burst(room, speaker).await;

                    // 전체에 FTKN 브로드캐스트 (요청자 포함)
                    let ftkn = build_mbcp_app(
                        config::MBCP_SUBTYPE_FTKN,
                        0, // 서버 SSRC = 0
                        Some(speaker),
                    );
                    self.broadcast_mbcp_to_subscribers(room, &ftkn);

                    // WS 호환: FLOOR_TAKEN 전송
                    self.send_ws_floor_taken(room, speaker, *priority);

                    if is_detail {
                        debug!("[MBCP] FTKN broadcast speaker={} priority={}", speaker, priority);
                    }
                }
                FloorAction::Denied { reason, current_speaker } => {
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
                FloorAction::Queued { user_id, position, priority, queue_size } => {
                    self.metrics.ptt_floor_queued.fetch_add(1, Ordering::Relaxed);
                    // MBCP UDP에서는 Queued에 대한 전용 subtype 없음
                    // Deny + 큐 정보를 FRVK로 전송 (WS에서 상세 처리)
                    let queued_msg = format!("queued: pos={} pri={} size={}", position, priority, queue_size);
                    let frvk = build_mbcp_app(
                        config::MBCP_SUBTYPE_FRVK,
                        0,
                        Some(&queued_msg),
                    );
                    self.send_mbcp_to_participant(requester, &frvk);

                    if is_detail {
                        debug!("[MBCP] queued user={} pos={}", user_id, position);
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
                    self.metrics.ptt_floor_revoked.fetch_add(1, Ordering::Relaxed);
                    if cause == "preempted" {
                        self.metrics.ptt_floor_preempted.fetch_add(1, Ordering::Relaxed);
                    }
                    // prev_speaker에게 FRVK
                    if let Some(p) = room.get_participant(prev_speaker) {
                        p.cancel_pli_burst();
                        let frvk = build_mbcp_app(
                            config::MBCP_SUBTYPE_FRVK,
                            0,
                            Some(cause),
                        );
                        self.send_mbcp_to_participant(&p, &frvk);
                    }
                    room.audio_rewriter.clear_speaker();
                    room.video_rewriter.clear_speaker();

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
    fn send_ws_floor_taken(&self, room: &Arc<Room>, speaker: &str, priority: u8) {
        let json = serde_json::json!({
            "op": crate::signaling::opcode::FLOOR_TAKEN,
            "pid": 0,
            "d": { "room_id": room.id, "speaker": speaker, "priority": priority },
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

        super::pli::spawn_pli_burst(&participant, ssrc, pub_addr, self.socket.clone(), &[0, 500, 1500], "MBCP");
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
        subscriber.pipeline.sub_nack_sent.fetch_add(parsed.nack_blocks.len() as u64, Ordering::Relaxed);
        for nack_block in &parsed.nack_blocks {
            self.handle_nack_block(nack_block, subscriber, room, is_detail);
        }

        // (2) RR 소비 (서버가 종단 — publisher에게 릴레이하지 않음)
        if !parsed.rr_blocks.is_empty() {
            self.metrics.rr_consumed.fetch_add(parsed.rr_blocks.len() as u64, Ordering::Relaxed);
            if is_detail {
                debug!("[RTCP:TERM] consumed {} RR block(s) from subscriber user={}",
                    parsed.rr_blocks.len(), subscriber.user_id);
            }
        }

        // (3) PLI/REMB 릴레이 → publisher별로 모아서 전송 (SR/RR 제외)
        if !parsed.relay_blocks.is_empty() {
            self.relay_subscribe_rtcp_blocks(&plaintext, &parsed, subscriber, room, is_detail).await;
        }

        // (4) MBCP APP 블록 — subscribe PC에서도 MBCP를 보낼 수 있음 (드문 경우)
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
        // RR relay count (PLI는 publisher 매칭 성공 후에만 카운트 — 유령 PLI 방지)
        for block in &parsed.relay_blocks {
            let pt = plaintext.get(block.offset + 1).copied().unwrap_or(0);
            if pt == config::RTCP_PT_RR { self.metrics.rr_relayed.fetch_add(1, Ordering::Relaxed); }
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
        let mut pli_per_ssrc: HashMap<u32, u64> = HashMap::new();
        for block in &parsed.relay_blocks {
            if block.media_ssrc == 0 { continue; }
            // Simulcast: 가상 video SSRC → 현재 레이어 실제 SSRC 역매핑
            let effective_ssrc = if room.simulcast_enabled {
                if let Some(pub_p) = room.find_publisher_by_vssrc(block.media_ssrc) {
                    let real_ssrc = {
                        let layers = _subscriber.subscribe_layers.lock().unwrap();
                        layers.get(&pub_p.user_id).and_then(|entry| {
                            let tracks = pub_p.tracks.lock().unwrap();
                            tracks.iter()
                                .find(|t| t.kind == TrackKind::Video && t.rid.as_deref() == Some(&entry.rid))
                                .map(|t| t.ssrc)
                        })
                    };
                    real_ssrc.unwrap_or(block.media_ssrc)
                } else {
                    block.media_ssrc
                }
            } else if ptt_audio_vssrc == Some(block.media_ssrc)
                || ptt_video_vssrc == Some(block.media_ssrc) {
                let is_video = ptt_video_vssrc == Some(block.media_ssrc);
                // current_speaker → last_speaker fallback (release 직후 PLI 대응)
                room.floor.current_speaker()
                    .or_else(|| room.floor.last_speaker())
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
            // PLI (PSFB) 블록인 경우 publisher별 카운트
            let pt = plaintext.get(block.offset + 1).copied().unwrap_or(0);
            if pt == config::RTCP_PT_PSFB {
                *pli_per_ssrc.entry(effective_ssrc).or_insert(0) += 1;
            }
        }

        for (media_ssrc, blocks) in &publisher_rtcp {
            let publisher = room.find_by_track_ssrc(*media_ssrc);

            let publisher = match publisher {
                Some(p) => p,
                None => {
                    let dropped_pli = pli_per_ssrc.get(media_ssrc).copied().unwrap_or(0);
                    if dropped_pli > 0 {
                        warn!("[RTCP:PLI] dropped — publisher not found ssrc=0x{:08X} ×{} room={} sub={}",
                            media_ssrc, dropped_pli, room.id, _subscriber.user_id);
                    } else if is_detail {
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
                // PLI per-publisher 계측
                let pli_count = pli_per_ssrc.get(media_ssrc).copied().unwrap_or(0);
                if pli_count > 0 {
                    self.metrics.pli_sent.fetch_add(pli_count, Ordering::Relaxed);
                    publisher.pipeline.pub_pli_received.fetch_add(pli_count, Ordering::Relaxed);
                    crate::agg_logger::inc_with(
                        crate::agg_logger::agg_key(&["pli_subscriber_relay", &room.id, &publisher.user_id]),
                        format!("pli_subscriber_relay pub={}", publisher.user_id),
                        Some(&room.id),
                    );
                }
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

            let (lookup_ssrc, cache_seqs) = if room.simulcast_enabled {
                // Simulcast: 가상 SSRC/seq → 실제 SSRC/seq 역매핑
                if let Some(pub_p) = room.find_publisher_by_vssrc(nack.media_ssrc) {
                    let (real_ssrc, real_seqs) = {
                        let layers = subscriber.subscribe_layers.lock().unwrap();
                        if let Some(entry) = layers.get(&pub_p.user_id) {
                            let seqs: Vec<u16> = lost_seqs.iter()
                                .map(|&vs| entry.rewriter.reverse_seq(vs))
                                .collect();
                            let ssrc = {
                                let tracks = pub_p.tracks.lock().unwrap();
                                tracks.iter()
                                    .find(|t| t.kind == TrackKind::Video && t.rid.as_deref() == Some(&entry.rid))
                                    .map(|t| t.ssrc)
                                    .unwrap_or(nack.media_ssrc)
                            };
                            (ssrc, seqs)
                        } else {
                            (nack.media_ssrc, lost_seqs.clone())
                        }
                    };
                    (real_ssrc, real_seqs)
                } else {
                    (nack.media_ssrc, lost_seqs.clone())
                }
            } else if ptt_virtual_video_ssrc == Some(nack.media_ssrc) {
                self.metrics.ptt_nack_remapped.fetch_add(1, Ordering::Relaxed);
                let original_seqs: Vec<u16> = lost_seqs.iter()
                    .map(|&vs| room.video_rewriter.reverse_seq(vs))
                    .collect();
                // current_speaker → last_speaker fallback (release 직후 NACK 대응)
                let speaker_uid = room.floor.current_speaker()
                    .or_else(|| room.floor.last_speaker());
                let speaker_ssrc = speaker_uid
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
                    crate::agg_logger::inc_with(
                        crate::agg_logger::agg_key(&["nack_pub_not_found", &room.id]),
                        format!("nack_pub_not_found ssrc=0x{:08X} user={}", lookup_ssrc, subscriber.user_id),
                        Some(&room.id),
                    );
                    continue;
                }
            };

            let rtx_ssrc = publisher.get_tracks().iter()
                .find(|t| t.ssrc == lookup_ssrc)
                .and_then(|t| t.rtx_ssrc);

            let rtx_ssrc = match rtx_ssrc {
                Some(s) => s,
                None => {
                    // audio SSRC에 대한 NACK은 정상 (audio에 RTX 없음) — 노이즈 억제
                    let is_audio = publisher.get_tracks().iter()
                        .find(|t| t.ssrc == lookup_ssrc)
                        .map(|t| t.kind == TrackKind::Audio)
                        .unwrap_or(false);
                    if !is_audio {
                        self.metrics.nack_no_rtx_ssrc.fetch_add(1, Ordering::Relaxed);
                        crate::agg_logger::inc_with(
                            crate::agg_logger::agg_key(&["nack_no_rtx_ssrc", &room.id]),
                            format!("nack_no_rtx_ssrc ssrc=0x{:08X} user={}", lookup_ssrc, subscriber.user_id),
                            Some(&room.id),
                        );
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

            if cache_miss > 0 {
                self.metrics.rtx_cache_miss.fetch_add(cache_miss as u64, Ordering::Relaxed);
                crate::agg_logger::inc_with(
                    crate::agg_logger::agg_key(&["rtx_cache_miss", &room.id]),
                    format!("rtx_cache_miss {}/{} ssrc=0x{:08X} user={}",
                        cache_miss, cache_seqs.len(), lookup_ssrc, publisher.user_id),
                    Some(&room.id),
                );
            }

            for (_lost_seq, _rtx_seq, rtx_pkt) in rtx_packets {
                // RTX budget: subscriber별 3초당 상한 초과 시 드롭 (다른 참가자 egress 큐 보호)
                let used = subscriber.rtx_budget_used.fetch_add(1, Ordering::Relaxed);
                if used >= config::RTX_BUDGET_PER_3S {
                    self.metrics.rtx_budget_exceeded.fetch_add(1, Ordering::Relaxed);
                    crate::agg_logger::inc_with(
                        crate::agg_logger::agg_key(&["rtx_budget_exceeded", &room.id]),
                        format!("rtx_budget_exceeded user={} used={}", subscriber.user_id, used),
                        Some(&room.id),
                    );
                    continue;
                }
                self.metrics.rtx_sent.fetch_add(1, Ordering::Relaxed);
                if subscriber.egress_tx.try_send(EgressPacket::Rtp(rtx_pkt)).is_err() {
                    self.metrics.egress_drop.fetch_add(1, Ordering::Relaxed);
                } else {
                    subscriber.pipeline.sub_rtx_received.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }

    // ========================================================================
    // Publish RTCP relay — SR translation + fan-out (Phase C-2a v2)
    // ========================================================================

    /// Publisher SR을 subscriber별로 변환하여 릴레이.
    ///
    /// SR 변환 전략:
    ///   - Conference: SSRC/NTP/RTP ts 원본, packet_count/octet_count만 egress 기준
    ///   - PTT: SSRC → 가상 SSRC, RTP ts → 오프셋 변환, counts → egress 기준
    ///   - NTP timestamp은 항상 원본 유지 (lip sync 기준점)
    ///
    /// relay_blocks (PLI/REMB)는 변환 없이 통과.
    fn relay_publish_rtcp_translated(
        &self,
        sr_blocks: &[Vec<u8>],
        relay_blocks: &[Vec<u8>],
        sender: &Arc<crate::room::participant::Participant>,
        room: &Arc<Room>,
        is_detail: bool,
    ) {
        // PTT 모드: floor holder만 릴레이 대상
        if room.mode == RoomMode::Ptt {
            let allowed = match room.floor.current_speaker() {
                Some(ref speaker) if speaker == &sender.user_id => true,
                _ => false,
            };
            if !allowed { return; }
        }

        // PTT 모드: SR 릴레이 중단
        // 화자 교대 시 NTP(실시간)는 idle 구간만큼 점프하는데 RTP(미디어 시간)는 연속 스트림으로 거의 안 점프
        // → NTP↔RTP 선형 관계 파괴 → Chrome jitter buffer가 버퍼를 계속 키움 → jb_delay 점진적 폭등
        // PTT에서는 1인 발화이므로 lip sync 불필요, arrival time 기반으로 충분
        let use_sr = room.mode != RoomMode::Ptt;

        for entry in room.participants.iter() {
            if entry.key() == &sender.user_id { continue; }
            let target = entry.value();
            if !target.is_subscribe_ready() { continue; }

            let mut compound_slices: Vec<Vec<u8>> = Vec::new();

            // Conference: subscriber별 SR 변환, PTT: SR 제외
            if use_sr {
                for sr_block in sr_blocks {
                    let tr = self.build_sr_translation(sr_block, sender, &target, room);
                    match rtcp_terminator::translate_sr(sr_block, &tr) {
                        Some(translated) => compound_slices.push(translated),
                        None => compound_slices.push(sr_block.clone()),
                    }
                }
            }

            // 비-SR relay 블록 (PLI/REMB) 통과
            for blk in relay_blocks {
                compound_slices.push(blk.clone());
            }

            if compound_slices.is_empty() { continue; }

            let refs: Vec<&[u8]> = compound_slices.iter().map(|v| v.as_slice()).collect();
            let compound = assemble_compound(&refs);

            self.metrics.sr_relayed.fetch_add(1, Ordering::Relaxed);

            if target.egress_tx.try_send(EgressPacket::Rtcp(compound)).is_err() {
                self.metrics.egress_drop.fetch_add(1, Ordering::Relaxed);
            } else {
                self.metrics.egress_rtcp_relayed.fetch_add(1, Ordering::Relaxed);
                target.pipeline.sub_sr_relayed.fetch_add(1, Ordering::Relaxed);
            }

            if is_detail {
                debug!("[RTCP:TERM] SR translated for subscriber user={}", target.user_id);
            }
        }
    }

    /// Publisher SR → subscriber SR 변환 파라미터 생성
    ///
    /// PTT: SSRC → virtual, RTP ts → 오프셋 변환, counts → egress
    /// Conference: counts만 egress 기준으로 교체
    fn build_sr_translation(
        &self,
        sr_block: &[u8],
        sender: &Arc<crate::room::participant::Participant>,
        target: &Arc<crate::room::participant::Participant>,
        room: &Arc<Room>,
    ) -> rtcp_terminator::SrTranslation {
        // SR에서 publisher SSRC와 RTP timestamp 추출
        let pub_ssrc = if sr_block.len() >= 8 {
            u32::from_be_bytes([sr_block[4], sr_block[5], sr_block[6], sr_block[7]])
        } else {
            0
        };
        let original_rtp_ts = if sr_block.len() >= 20 {
            u32::from_be_bytes([sr_block[16], sr_block[17], sr_block[18], sr_block[19]])
        } else {
            0
        };

        if room.mode == RoomMode::Ptt {
            // publisher SSRC → audio/video 판별
            let is_audio = {
                let tracks = sender.tracks.lock().unwrap();
                tracks.iter().any(|t| t.ssrc == pub_ssrc && t.kind == TrackKind::Audio)
            };

            let (virtual_ssrc, translated_rtp_ts) = if is_audio {
                (
                    room.audio_rewriter.virtual_ssrc(),
                    room.audio_rewriter.translate_rtp_ts(original_rtp_ts),
                )
            } else {
                (
                    room.video_rewriter.virtual_ssrc(),
                    room.video_rewriter.translate_rtp_ts(original_rtp_ts),
                )
            };

            // subscriber의 send_stats에서 가상 SSRC 기준 counts 조회
            let (pkt, oct) = {
                let stats_map = target.send_stats.lock().unwrap();
                stats_map.get(&virtual_ssrc)
                    .map(|s| (s.packets_sent, s.bytes_sent))
                    .unwrap_or((0, 0))
            };

            rtcp_terminator::SrTranslation {
                ssrc: Some(virtual_ssrc),
                rtp_ts: translated_rtp_ts,
                packet_count: pkt,
                octet_count: oct,
            }
        } else {
            // Simulcast video: real SSRC → virtual SSRC 변환 + send_stats 조회
            if room.simulcast_enabled {
                let is_video = {
                    let tracks = sender.tracks.lock().unwrap();
                    tracks.iter().any(|t| t.ssrc == pub_ssrc && t.kind == TrackKind::Video)
                };
                if is_video {
                    let vssrc = sender.ensure_simulcast_video_ssrc();
                    let (pkt, oct) = {
                        let stats_map = target.send_stats.lock().unwrap();
                        stats_map.get(&vssrc)
                            .map(|s| (s.packets_sent, s.bytes_sent))
                            .unwrap_or((0, 0))
                    };
                    return rtcp_terminator::SrTranslation {
                        ssrc: Some(vssrc),
                        rtp_ts: None,
                        packet_count: pkt,
                        octet_count: oct,
                    };
                }
            }
            // Non-simulcast 또는 audio: 기존 동작
            let (pkt, oct) = {
                let stats_map = target.send_stats.lock().unwrap();
                stats_map.get(&pub_ssrc)
                    .map(|s| (s.packets_sent, s.bytes_sent))
                    .unwrap_or((0, 0))
            };

            rtcp_terminator::SrTranslation {
                ssrc: None,
                rtp_ts: None,
                packet_count: pkt,
                octet_count: oct,
            }
        }
    }
}
