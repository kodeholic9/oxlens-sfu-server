// author: kodeholic (powered by Claude)
//! PLI burst — 공통 PLI burst spawn 헬퍼
//!
//! SFU 내 PLI burst 패턴은 5곳 이상에서 반복됨:
//! - handler/track_ops.rs: CAMERA_READY, SUBSCRIBE_LAYER
//! - handler/floor_ops.rs: FLOOR_REQUEST
//! - ingress.rs: fanout_simulcast_video, send_pli_burst (MBCP)
//!
//! 모두 동일한 구조: cancel → spawn(delays loop → encrypt → send) → store abort handle
//! delays 배열만 상황에 따라 다름.

use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::time::Duration;
use tracing::{info, warn};

use crate::room::participant::Participant;

/// PLI burst를 비동기 태스크로 spawn.
///
/// - 기존 진행 중인 PLI burst가 있으면 cancel
/// - `delays`에 지정된 ms 간격으로 PLI 전송 (첫 번째가 0이면 즉시)
/// - abort handle을 participant에 저장하여 퇴장 시 cancel 가능
///
/// # Arguments
/// - `participant` — PLI를 보낼 대상 (publish session 사용)
/// - `ssrc` — PLI 대상 미디어 SSRC
/// - `pub_addr` — publisher의 UDP 주소
/// - `socket` — 공유 UDP 소켓
/// - `delays` — 각 PLI 전송 간격 (ms). 예: &[0, 500, 1500]
/// - `log_prefix` — 로그 식별자. 예: "CAMERA_READY", "FLOOR", "SIM:PLI"
pub fn spawn_pli_burst(
    participant: &Arc<Participant>,
    ssrc: u32,
    pub_addr: SocketAddr,
    socket: Arc<UdpSocket>,
    delays: &[u64],
    log_prefix: &str,
) {
    participant.cancel_pli_burst();

    let p = Arc::clone(participant);
    let uid = participant.user_id.clone();
    let prefix = log_prefix.to_string();
    let delays_owned: Vec<u64> = delays.to_vec();

    let handle = tokio::spawn(async move {
        for (i, &delay_ms) in delays_owned.iter().enumerate() {
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
                    warn!("[{}] PLI send FAILED user={} ssrc=0x{:08X} #{}: {e}",
                        prefix, uid, ssrc, i);
                    break;
                } else {
                    info!("[{}] PLI sent user={} ssrc=0x{:08X} #{}",
                        prefix, uid, ssrc, i);
                }
            }
        }
    });

    *participant.pli_burst_handle.lock().unwrap() = Some(handle.abort_handle());
}
