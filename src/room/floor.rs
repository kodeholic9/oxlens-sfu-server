// author: kodeholic (powered by Claude)
//! Floor Control — MCPTT/MBCP 기반 발화권 중재 (Phase 1)
//!
//! 3GPP TS 24.380 Floor Control 용어:
//!   - Floor Participant: 클라이언트 (발화권 요청/수신)
//!   - Floor Control Server: 서버 (발화권 중재/허가)
//!   - Talk Burst: 1회 연속 발화 구간
//!
//! 서버 상태 머신 (간소화):
//!   IDLE (아무도 발화 안 함)
//!     → Floor Request 수신 → TAKEN
//!   TAKEN (누군가 발화 중)
//!     → Floor Release 수신 → IDLE
//!     → Floor Request (다른 사용자) → Deny
//!     → T2 타임아웃 (max talk burst) → Revoke → IDLE
//!     → T_FLOOR_TIMEOUT (ping 미수신) → Revoke → IDLE

use std::sync::Mutex;
use tracing::{info, warn};

use crate::config;

// ============================================================================
// FloorState
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FloorState {
    /// 채널 비어있음, 발화자 없음
    Idle,
    /// 누군가 발화 중
    Taken {
        /// 현재 발화자 user_id
        speaker: String,
        /// Talk Burst 시작 시각 (ms, epoch)
        burst_start: u64,
        /// 마지막 Floor PING 수신 시각 (ms, epoch)
        last_ping: u64,
    },
}

// ============================================================================
// FloorAction — 핸들러에게 돌려주는 지시
// ============================================================================

/// FloorController가 반환하는 액션. 핸들러가 이에 따라 메시지를 전송한다.
#[derive(Debug)]
pub enum FloorAction {
    /// 발화권 허가 — 요청자에게 Granted, 전체에 Floor Taken
    Granted { speaker: String },
    /// 발화권 거부 — 요청자에게 Deny
    Denied { reason: String, current_speaker: String },
    /// 발화권 해제 — 전체에 Floor Idle
    Released { prev_speaker: String },
    /// 서버 강제 회수 — 발화자에게 Revoke, 전체에 Floor Idle
    Revoked { prev_speaker: String, cause: String },
    /// Floor PING 정상 응답
    PingOk,
    /// Floor PING 거부 (발화자가 아닌 클라이언트가 보냄)
    PingDenied,
    /// 해당 요청이 무효 (PTT 모드가 아닌 room 등)
    NotPttRoom,
}

// ============================================================================
// FloorController
// ============================================================================

/// Room 단위 Floor Control 상태 머신.
/// signaling handler에서 호출하고, 반환된 FloorAction에 따라 메시지 전송.
pub struct FloorController {
    state: Mutex<FloorState>,
    /// 직전 발화자 (발화 종료 후 NACK 역매핑용)
    /// Idle 전환 시 저장, 다음 Granted 시 초기화
    prev_speaker: Mutex<Option<String>>,
}

impl FloorController {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(FloorState::Idle),
            prev_speaker: Mutex::new(None),
        }
    }

    /// 현재 Floor 상태 스냅샷
    pub fn current_state(&self) -> FloorState {
        self.state.lock().unwrap().clone()
    }

    /// 현재 발화자 user_id (없으면 None)
    pub fn current_speaker(&self) -> Option<String> {
        match &*self.state.lock().unwrap() {
            FloorState::Taken { speaker, .. } => Some(speaker.clone()),
            FloorState::Idle => None,
        }
    }

    /// 직전 발화자 (Idle 전환 후에도 유효, NACK 역매핑 fallback용)
    pub fn last_speaker(&self) -> Option<String> {
        self.prev_speaker.lock().unwrap().clone()
    }

    /// Floor Request — 발화권 요청 (PTT 누름)
    pub fn request(&self, user_id: &str, now_ms: u64) -> FloorAction {
        let mut state = self.state.lock().unwrap();
        match &*state {
            FloorState::Idle => {
                info!("[FLOOR] granted user={}", user_id);
                *state = FloorState::Taken {
                    speaker: user_id.to_string(),
                    burst_start: now_ms,
                    last_ping: now_ms,
                };
                // 새 화자 시작 → prev_speaker 초기화 (이제 현재 화자가 있으니까)
                *self.prev_speaker.lock().unwrap() = None;
                FloorAction::Granted {
                    speaker: user_id.to_string(),
                }
            }
            FloorState::Taken { speaker, .. } => {
                if speaker == user_id {
                    // 이미 발화 중인 사용자가 다시 요청 — 무시 (idempotent)
                    FloorAction::Granted {
                        speaker: user_id.to_string(),
                    }
                } else {
                    info!("[FLOOR] denied user={} (speaker={})", user_id, speaker);
                    FloorAction::Denied {
                        reason: "channel busy".to_string(),
                        current_speaker: speaker.clone(),
                    }
                }
            }
        }
    }

    /// Floor Release — 발화권 자진 해제 (PTT 뗌)
    pub fn release(&self, user_id: &str) -> FloorAction {
        let mut state = self.state.lock().unwrap();
        match &*state {
            FloorState::Taken { speaker, .. } if speaker == user_id => {
                info!("[FLOOR] released user={}", user_id);
                *self.prev_speaker.lock().unwrap() = Some(user_id.to_string());
                *state = FloorState::Idle;
                FloorAction::Released {
                    prev_speaker: user_id.to_string(),
                }
            }
            FloorState::Taken { speaker, .. } => {
                // 발화자가 아닌 사용자가 release — 무시
                warn!("[FLOOR] release ignored user={} (speaker={})", user_id, speaker);
                FloorAction::PingDenied
            }
            FloorState::Idle => {
                // 이미 Idle — 무시 (idempotent)
                FloorAction::Released {
                    prev_speaker: user_id.to_string(),
                }
            }
        }
    }

    /// Floor PING — 발화자 생존 확인
    pub fn ping(&self, user_id: &str, now_ms: u64) -> FloorAction {
        let mut state = self.state.lock().unwrap();
        match &mut *state {
            FloorState::Taken { speaker, last_ping, .. } if speaker == user_id => {
                *last_ping = now_ms;
                FloorAction::PingOk
            }
            _ => FloorAction::PingDenied,
        }
    }

    /// 타이머 체크 — 주기적으로 호출 (reaper task 등에서)
    /// T2(max burst) 및 T_FLOOR_TIMEOUT(ping 미수신) 확인
    /// revoke 필요 시 FloorAction::Revoked 반환
    pub fn check_timers(&self, now_ms: u64) -> Option<FloorAction> {
        let mut state = self.state.lock().unwrap();
        match &*state {
            FloorState::Taken { speaker, burst_start, last_ping } => {
                // T2: max talk burst 초과
                if now_ms.saturating_sub(*burst_start) > config::FLOOR_MAX_BURST_MS {
                    let prev = speaker.clone();
                    warn!("[FLOOR] T2 expired user={} burst={}ms",
                        prev, now_ms.saturating_sub(*burst_start));
                    *self.prev_speaker.lock().unwrap() = Some(prev.clone());
                    *state = FloorState::Idle;
                    return Some(FloorAction::Revoked {
                        prev_speaker: prev,
                        cause: "max burst exceeded".to_string(),
                    });
                }
                // T_FLOOR_TIMEOUT: ping 미수신
                if now_ms.saturating_sub(*last_ping) > config::FLOOR_PING_TIMEOUT_MS {
                    let prev = speaker.clone();
                    warn!("[FLOOR] ping timeout user={} last_ping={}ms ago",
                        prev, now_ms.saturating_sub(*last_ping));
                    *self.prev_speaker.lock().unwrap() = Some(prev.clone());
                    *state = FloorState::Idle;
                    return Some(FloorAction::Revoked {
                        prev_speaker: prev,
                        cause: "participant lost".to_string(),
                    });
                }
                None
            }
            FloorState::Idle => None,
        }
    }

    /// 참가자 퇴장 시 — 발화자였으면 자동 release
    pub fn on_participant_leave(&self, user_id: &str) -> Option<FloorAction> {
        let mut state = self.state.lock().unwrap();
        match &*state {
            FloorState::Taken { speaker, .. } if speaker == user_id => {
                info!("[FLOOR] auto-release on leave user={}", user_id);
                *self.prev_speaker.lock().unwrap() = Some(user_id.to_string());
                *state = FloorState::Idle;
                Some(FloorAction::Released {
                    prev_speaker: user_id.to_string(),
                })
            }
            _ => None,
        }
    }
}
