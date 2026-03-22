// author: kodeholic (powered by Claude)
//! Egress — subscriber 방향 패킷 송신 처리
//!
//! - send_twcc_to_publishers: TWCC feedback → publisher (BWE_MODE=twcc, 100ms)
//! - send_remb_to_publishers: REMB 힌트 → publisher (BWE_MODE=remb, 1초)
//! - run_egress_task: subscriber별 전용 egress (큐 → encrypt → send)
//! - send_pli_to_publishers: subscribe ready → PLI 요청

use std::sync::Arc;
use std::time::Instant;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::room::participant::{EgressPacket, TrackKind};

use super::UdpTransport;
use crate::metrics::GlobalMetrics;
use super::rtcp::{build_pli, build_remb};
use super::rtcp_terminator;
use super::twcc::build_twcc_feedback;

// ============================================================================
// UdpTransport impl — BWE feedback 전송 (TWCC 100ms / REMB 1초)
// ============================================================================

impl UdpTransport {
    /// 모든 room의 모든 publisher에게 TWCC feedback 전송 (100ms 주기)
    /// Chrome GCC가 패킷 도착 시간 변화(delay gradient)를 분석해 비트레이트 자율 결정
    pub(crate) async fn send_twcc_to_publishers(&self) {
        for room_entry in self.room_hub.rooms.iter() {
            let room = room_entry.value();

            for entry in room.participants.iter() {
                let publisher = entry.value();
                if !publisher.is_publish_ready() { continue; }

                let pub_addr = match publisher.publish.get_address() {
                    Some(a) => a,
                    None => continue,
                };

                // video 트랙 SSRC 찾기 (TWCC feedback의 media_ssrc)
                let video_ssrc = {
                    let tracks = publisher.tracks.lock().unwrap();
                    tracks.iter()
                        .find(|t| t.kind == TrackKind::Video)
                        .map(|t| t.ssrc)
                };

                let ssrc = match video_ssrc {
                    Some(s) => s,
                    None => continue,
                };

                // TwccRecorder에서 feedback 생성
                let feedback = {
                    let mut rec = publisher.twcc_recorder.lock().unwrap();
                    build_twcc_feedback(&mut rec, ssrc)
                };

                let feedback_plain = match feedback {
                    Some(f) => f,
                    None => continue, // pending 패킷 없음
                };

                // SRTCP 암호화 (publisher의 publish session outbound context)
                let encrypted = {
                    let mut ctx = publisher.publish.outbound_srtp.lock().unwrap();
                    match ctx.encrypt_rtcp(&feedback_plain) {
                        Ok(p) => p,
                        Err(_) => continue,
                    }
                };

                if let Err(e) = self.socket.send_to(&encrypted, pub_addr).await {
                    debug!("[TWCC] send FAILED user={} addr={}: {e}", publisher.user_id, pub_addr);
                } else {
                    self.metrics.twcc_sent.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            }
        }
    }

    /// 모든 room의 모든 publisher에게 REMB 전송 (BWE_MODE=remb 시 사용)
    /// Chrome BWE에게 "이 만큼까지 보내도 된다"는 고정 대역폭 힌트 제공
    pub(crate) async fn send_remb_to_publishers(&self) {
        for room_entry in self.room_hub.rooms.iter() {
            let room = room_entry.value();

            for entry in room.participants.iter() {
                let publisher = entry.value();
                if !publisher.is_publish_ready() { continue; }

                let pub_addr = match publisher.publish.get_address() {
                    Some(a) => a,
                    None => continue,
                };

                let video_ssrc = {
                    let tracks = publisher.tracks.lock().unwrap();
                    tracks.iter()
                        .find(|t| t.kind == TrackKind::Video)
                        .map(|t| t.ssrc)
                };

                let ssrc = match video_ssrc {
                    Some(s) => s,
                    None => continue,
                };

                let remb_plain = build_remb(self.remb_bitrate, ssrc);

                let encrypted = {
                    let mut ctx = publisher.publish.outbound_srtp.lock().unwrap();
                    match ctx.encrypt_rtcp(&remb_plain) {
                        Ok(p) => p,
                        Err(_) => continue,
                    }
                };

                if let Err(e) = self.socket.send_to(&encrypted, pub_addr).await {
                    debug!("[REMB] send FAILED user={} addr={}: {e}", publisher.user_id, pub_addr);
                } else {
                    self.metrics.remb_sent.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            }
        }
    }
}

// ============================================================================
// Phase W-3: Subscriber Egress Task (LiveKit 패턴)
// ============================================================================

/// Subscriber별 전용 egress task — 큐에서 plaintext를 꺼내서 encrypt → send
/// subscriber당 1개, outbound_srtp를 독점 → Mutex 경합 없음
pub(crate) async fn run_egress_task(
    mut rx: mpsc::Receiver<EgressPacket>,
    participant: Arc<crate::room::participant::Participant>,
    socket: Arc<UdpSocket>,
    metrics: Arc<GlobalMetrics>,
) {
    info!("[EGRESS] started user={}", participant.user_id);

    while let Some(pkt) = rx.recv().await {
        let addr = match participant.subscribe.get_address() {
            Some(a) => a,
            None => continue,
        };

        let t0 = Instant::now();
        let result = match pkt {
            EgressPacket::Rtp(ref plaintext) => {
                // RTCP Terminator: SendStats 갱신 (SR translation용)
                // RTX는 재전송이므로 제외
                if plaintext.len() >= 12 {
                    let pt = plaintext[1] & 0x7F;
                    if !crate::config::is_rtx_pt(pt) {
                        let ssrc = u32::from_be_bytes(
                            [plaintext[8], plaintext[9], plaintext[10], plaintext[11]]
                        );
                        let rtp_ts = u32::from_be_bytes(
                            [plaintext[4], plaintext[5], plaintext[6], plaintext[7]]
                        );
                        // payload len: 헤더(12 + CSRC + ext) 제외
                        let cc = (plaintext[0] & 0x0F) as usize;
                        let has_ext = (plaintext[0] & 0x10) != 0;
                        let mut hdr_len = 12 + cc * 4;
                        if has_ext && plaintext.len() >= hdr_len + 4 {
                            let ext_words = u16::from_be_bytes(
                                [plaintext[hdr_len + 2], plaintext[hdr_len + 3]]
                            ) as usize;
                            hdr_len += 4 + ext_words * 4;
                        }
                        let payload_len = plaintext.len().saturating_sub(hdr_len);

                        let clock_rate = if crate::config::is_audio_pt(pt) {
                            crate::config::CLOCK_RATE_AUDIO
                        } else {
                            crate::config::CLOCK_RATE_VIDEO
                        };
                        let mut stats_map = participant.send_stats.lock().unwrap();
                        let stats = stats_map.entry(ssrc)
                            .or_insert_with(|| rtcp_terminator::SendStats::new(ssrc, clock_rate));
                        stats.on_rtp_sent(rtp_ts, payload_len);
                    }
                }
                let mut ctx = participant.subscribe.outbound_srtp.lock().unwrap();
                ctx.encrypt_rtp(plaintext)
            }
            EgressPacket::Rtcp(ref plaintext) => {
                let mut ctx = participant.subscribe.outbound_srtp.lock().unwrap();
                ctx.encrypt_rtcp(plaintext)
            }
        };
        metrics.egress_encrypt.record(t0.elapsed().as_micros() as u64);

        if let Ok(encrypted) = result {
            let _ = socket.send_to(&encrypted, addr).await;
        }
    }

    info!("[EGRESS] ended user={}", participant.user_id);
}

// ============================================================================
// RTCP Terminator: 서버 자체 RR/SR 생성 + 전송 (1초 주기)
// ============================================================================

impl UdpTransport {
    /// 모든 room의 모든 publisher에게 서버 자체 RR 전송
    /// SFU는 publisher의 RTP를 수신하는 peer이므로, 수신 통계 기반 RR을 직접 생성한다.
    /// subscriber의 RR은 publisher에게 릴레이하지 않는다 (ingress에서 소비).
    pub(crate) async fn send_rtcp_reports(&self) {
        // 서버 자체 SSRC (RR sender SSRC — 임의 고정값)
        const SERVER_SSRC: u32 = 0x00000001;

        for room_entry in self.room_hub.rooms.iter() {
            let room = room_entry.value();

            for entry in room.participants.iter() {
                let publisher = entry.value();
                if !publisher.is_publish_ready() { continue; }

                let pub_addr = match publisher.publish.get_address() {
                    Some(a) => a,
                    None => continue,
                };

                // publisher의 recv_stats에서 RR Report Block 생성
                let rr_blocks: Vec<rtcp_terminator::RrReportBlock> = {
                    let mut stats_map = publisher.recv_stats.lock().unwrap();
                    stats_map.values_mut()
                        .map(|stats| stats.build_rr_block())
                        .collect()
                };

                if rr_blocks.is_empty() { continue; }

                // RTCP Terminator 진단: RR 블록 데이터를 메트릭 스냅샷으로 전송
                {
                    let mut diag = self.metrics.rr_diag_snapshot.lock().unwrap();
                    for block in &rr_blocks {
                        diag.push(serde_json::json!({
                            "user": publisher.user_id,
                            "ssrc": format!("0x{:08X}", block.ssrc),
                            "frac_lost": block.fraction_lost,
                            "cum_lost": block.cumulative_lost,
                            "ext_seq": block.extended_highest_seq,
                            "jitter": block.jitter,
                            "lsr": format!("0x{:08X}", block.last_sr),
                            "dlsr": block.delay_since_last_sr,
                        }));
                    }
                }

                let rr_plain = rtcp_terminator::build_receiver_report(SERVER_SSRC, &rr_blocks);

                // SRTCP 암호화 (publisher의 publish session outbound context)
                let encrypted = {
                    let mut ctx = publisher.publish.outbound_srtp.lock().unwrap();
                    match ctx.encrypt_rtcp(&rr_plain) {
                        Ok(p) => p,
                        Err(_) => continue,
                    }
                };

                if let Err(e) = self.socket.send_to(&encrypted, pub_addr).await {
                    warn!("[RTCP:TERM] RR send FAILED user={} addr={}: {e}",
                        publisher.user_id, pub_addr);
                } else {
                    self.metrics.rr_generated.fetch_add(
                        rr_blocks.len() as u64,
                        std::sync::atomic::Ordering::Relaxed,
                    );
                    debug!("[RTCP:TERM] RR sent user={} blocks={} addr={}",
                        publisher.user_id, rr_blocks.len(), pub_addr);
                }

                // --- SR 생성: 비활성 ---
                // 서버 자체 클록으로 SR을 만들면 NTP/RTP timestamp이
                // 원본 미디어 소스와 안 맞아서 jb_delay 폭등.
                // publisher SR을 subscriber에게 릴레이하는 방식으로 복원 (ingress에서 처리).
                // TODO: publisher SR 기반 NTP/RTP translation 설계 후 재활성화
            }
        }
    }
}

/// Subscribe SRTP ready 시 해당 room의 모든 publisher에게 PLI 전송
pub(crate) async fn send_pli_to_publishers(
    socket: &UdpSocket,
    room: &crate::room::room::Room,
    exclude_user: &str,
) {
    for entry in room.participants.iter() {
        let publisher = entry.value();
        if publisher.user_id == exclude_user { continue; }
        if !publisher.is_publish_ready() { continue; }

        let pub_addr = match publisher.publish.get_address() {
            Some(a) => a,
            None => continue,
        };

        // publisher의 video 트랙 SSRC 찾기
        let video_ssrc = {
            let tracks = publisher.tracks.lock().unwrap();
            tracks.iter()
                .find(|t| t.kind == TrackKind::Video)
                .map(|t| t.ssrc)
        };

        let ssrc = match video_ssrc {
            Some(s) => s,
            None => continue, // video 트랙 없으면 skip
        };

        let pli_plain = build_pli(ssrc);

        // SRTCP 암호화 (publisher의 publish session outbound context)
        let encrypted = {
            let mut ctx = publisher.publish.outbound_srtp.lock().unwrap();
            match ctx.encrypt_rtcp(&pli_plain) {
                Ok(p) => p,
                Err(e) => {
                    warn!("[DBG:PLI] encrypt FAILED → user={}: {e}", publisher.user_id);
                    continue;
                }
            }
        };

        if let Err(e) = socket.send_to(&encrypted, pub_addr).await {
            warn!("[DBG:PLI] send FAILED → user={} addr={}: {e}", publisher.user_id, pub_addr);
        } else {
            publisher.pipeline.pub_pli_received.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            crate::agg_logger::inc_with(
                crate::agg_logger::agg_key(&["pli_server_initiated", &room.id, &publisher.user_id]),
                format!("pli_server_subscribe_ready pub={}", publisher.user_id),
                Some(&room.id),
            );
            info!("[DBG:PLI] sent → user={} ssrc=0x{:08X} addr={}",
                publisher.user_id, ssrc, pub_addr);
        }
    }
}
