// author: kodeholic (powered by Claude)
//! Ingress — publish RTP/RTCP 수신 처리 (hot path)
//!
//! - handle_srtp: publish RTP decrypt → RTP cache → egress 큐 fan-out
//! - handle_subscribe_rtcp: subscribe RTCP → NACK/RR/PLI/REMB 분기
//! - handle_nack_block: NACK → cache 조회 → RTX 조립 → egress 큐
//! - relay_publish_rtcp: publish RTCP(SR) → egress 큐 fan-out

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;
use tracing::{debug, trace};

use crate::config;
use crate::room::participant::{PcType, EgressPacket};
use crate::room::room::Room;

use super::UdpTransport;
use super::rtcp::{
    parse_rtp_header, current_ts, split_compound_rtcp, parse_rtcp_nack,
    expand_nack, build_rtx_packet, assemble_compound,
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

            // Phase C-2a: SR relay — publisher의 SR을 모든 subscriber에게 전달
            self.relay_publish_rtcp(&plaintext, &sender, &room, is_detail);
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
            if target.egress_tx.try_send(EgressPacket::Rtp(plaintext.clone())).is_err() {
                self.metrics.egress_drop.fetch_add(1, Ordering::Relaxed);
            }
        }
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
            debug!("[DBG:RTCP:SUB] user={} compound_len={} nack_blocks={} relay_blocks={}",
                subscriber.user_id, plaintext.len(), parsed.nack_blocks.len(), parsed.relay_blocks.len());
        }

        // (1) NACK 처리 (RTX 재전송 — 기존 로직)
        self.metrics.nack_received.fetch_add(parsed.nack_blocks.len() as u64, Ordering::Relaxed);
        for nack_block in &parsed.nack_blocks {
            self.handle_nack_block(nack_block, subscriber, room, is_detail);
        }

        // (2) 릴레이 대상 RTCP (RR, PLI, REMB) → publisher별로 모아서 전송
        if parsed.relay_blocks.is_empty() {
            return;
        }

        // RR/PLI relay count — plaintext는 이미 복호화된 RTCP이므로 & 0x7F 마스크 불필요
        for block in &parsed.relay_blocks {
            let pt = plaintext.get(block.offset + 1).copied().unwrap_or(0);
            if pt == config::RTCP_PT_RR { self.metrics.rr_relayed.fetch_add(1, Ordering::Relaxed); }
            if pt == config::RTCP_PT_PSFB { self.metrics.pli_sent.fetch_add(1, Ordering::Relaxed); }
        }

        // media_ssrc → publisher 매핑 + RTCP 블록 그룹핑
        let mut publisher_rtcp: HashMap<u32, Vec<&[u8]>> = HashMap::new();
        for block in &parsed.relay_blocks {
            if block.media_ssrc == 0 { continue; } // RC=0 RR 등 media_ssrc 없는 경우 skip
            publisher_rtcp.entry(block.media_ssrc)
                .or_default()
                .push(&plaintext[block.offset..block.offset + block.length]);
        }

        for (media_ssrc, blocks) in &publisher_rtcp {
            // publisher 찾기 (zero-alloc DashMap iter)
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

            // 릴레이 대상 RTCP 블록들을 compound로 재조립
            let compound = assemble_compound(blocks);

            // publisher의 publish session outbound_srtp로 encrypt
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

        for nack in &nack_items {
            let lost_seqs = expand_nack(nack.pid, nack.blp);

            if is_detail {
                debug!("[DBG:NACK] user={} media_ssrc=0x{:08X} pid={} blp=0x{:04X} seqs={:?}",
                    subscriber.user_id, nack.media_ssrc, nack.pid, nack.blp, lost_seqs);
            }

            // 해당 media_ssrc의 publisher 찾기 (zero-alloc DashMap iter)
            let publisher = room.find_by_track_ssrc(nack.media_ssrc);

            let publisher = match publisher {
                Some(p) => p,
                None => {
                    self.metrics.nack_publisher_not_found.fetch_add(1, Ordering::Relaxed);
                    if is_detail {
                        debug!("[DBG:NACK] publisher not found for ssrc=0x{:08X}", nack.media_ssrc);
                    }
                    continue;
                }
            };

            // RTX SSRC 찾기
            let rtx_ssrc = publisher.get_tracks().iter()
                .find(|t| t.ssrc == nack.media_ssrc)
                .and_then(|t| t.rtx_ssrc);

            let rtx_ssrc = match rtx_ssrc {
                Some(s) => s,
                None => {
                    self.metrics.nack_no_rtx_ssrc.fetch_add(1, Ordering::Relaxed);
                    if is_detail {
                        debug!("[DBG:NACK] no rtx_ssrc for ssrc=0x{:08X}", nack.media_ssrc);
                    }
                    continue;
                }
            };

            // 캐시 조회 + RTX 조립
            let rtx_packets: Vec<(u16, u16, Vec<u8>)> = {
                let cache = publisher.rtp_cache.lock().unwrap();
                lost_seqs.iter().filter_map(|&lost_seq| {
                    let original = cache.get(lost_seq)?;
                    let rtx_seq = publisher.next_rtx_seq();
                    let rtx_pkt = build_rtx_packet(original, rtx_ssrc, rtx_seq);
                    Some((lost_seq, rtx_seq, rtx_pkt))
                }).collect()
            };

            let cache_miss = lost_seqs.len() - rtx_packets.len();
            self.metrics.rtx_cache_miss.fetch_add(cache_miss as u64, Ordering::Relaxed);
            self.metrics.rtx_sent.fetch_add(rtx_packets.len() as u64, Ordering::Relaxed);

            if cache_miss > 0 {
                // 진단 로그: miss된 seq와 캐시 슬롯 상태 확인 (처음 10건)
                if self.metrics.rtx_cache_miss.load(Ordering::Relaxed) <= 10 {
                    let cache = publisher.rtp_cache.lock().unwrap();
                    let missed: Vec<String> = lost_seqs.iter()
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
                        cache_miss, lost_seqs.len(), nack.media_ssrc,
                        publisher.user_id, self.metrics.rtp_cache_stored.load(Ordering::Relaxed), missed.join(", "));
                }
            }

            // Phase W-3: RTX도 egress 큐 경유
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
        self.metrics.sr_relayed.fetch_add(1, Ordering::Relaxed);

        // Phase W-3: egress 큐로 SR relay 전달 (DashMap iter 직접 순회, Vec 할당 없음)
        let plaintext_owned = plaintext.to_vec();

        for entry in room.participants.iter() {
            if entry.key() == &sender.user_id { continue; }
            let target = entry.value();
            if !target.is_subscribe_ready() { continue; }
            if target.egress_tx.try_send(EgressPacket::Rtcp(plaintext_owned.clone())).is_err() {
                self.metrics.egress_drop.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}
