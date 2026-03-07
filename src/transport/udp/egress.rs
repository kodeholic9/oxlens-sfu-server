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
use super::metrics::EgressTimingAtomics;
use super::rtcp::{build_pli, build_remb};
use super::twcc::build_twcc_feedback;

// ============================================================================
// UdpTransport impl — BWE feedback 전송 (TWCC 100ms / REMB 1초)
// ============================================================================

impl UdpTransport {
    /// 모든 room의 모든 publisher에게 TWCC feedback 전송 (100ms 주기)
    /// Chrome GCC가 패킷 도착 시간 변화(delay gradient)를 분석해 비트레이트 자율 결정
    pub(crate) async fn send_twcc_to_publishers(&mut self) {
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
                    self.metrics.twcc_sent += 1;
                }
            }
        }
    }

    /// 모든 room의 모든 publisher에게 REMB 전송 (BWE_MODE=remb 시 사용)
    /// Chrome BWE에게 "이 만큼까지 보내도 된다"는 고정 대역폭 힌트 제공
    pub(crate) async fn send_remb_to_publishers(&mut self) {
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
    timing: Arc<EgressTimingAtomics>,
) {
    info!("[EGRESS] started user={}", participant.user_id);

    while let Some(pkt) = rx.recv().await {
        let addr = match participant.subscribe.get_address() {
            Some(a) => a,
            None => continue,
        };

        let t0 = Instant::now();
        let result = match pkt {
            EgressPacket::Rtp(plaintext) => {
                let mut ctx = participant.subscribe.outbound_srtp.lock().unwrap();
                ctx.encrypt_rtp(&plaintext)
            }
            EgressPacket::Rtcp(plaintext) => {
                let mut ctx = participant.subscribe.outbound_srtp.lock().unwrap();
                ctx.encrypt_rtcp(&plaintext)
            }
        };
        timing.record(t0.elapsed().as_micros() as u64);

        if let Ok(encrypted) = result {
            let _ = socket.send_to(&encrypted, addr).await;
        }
    }

    info!("[EGRESS] ended user={}", participant.user_id);
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
            info!("[DBG:PLI] sent → user={} ssrc=0x{:08X} addr={}",
                publisher.user_id, ssrc, pub_addr);
        }
    }
}
