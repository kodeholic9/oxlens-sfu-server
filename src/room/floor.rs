// author: kodeholic (powered by Claude)
//! Floor Control v2 — 우선순위 + 큐잉 (3GPP TS 24.380 기반)
//!
//! 3GPP TS 24.380 Floor Control 용어:
//!   - Floor Participant: 클라이언트 (발화권 요청/수신)
//!   - Floor Control Server: 서버 (발화권 중재/허가)
//!   - Talk Burst: 1회 연속 발화 구간
//!   - Floor Priority: 0(최저)~255(최고), 기본 0
//!   - Preemption: 높은 우선순위 요청 시 현재 발화자 강제 교체
//!   - Queuing: 발화권 획득 실패 시 대기열 삽입
//!
//! 서버 상태 머신 (3GPP §6.3.4 기반):
//!   G:Idle → Floor Request → G:Taken (grant)
//!   G:Taken → Floor Request (higher priority) → preempt → G:Taken (new)
//!   G:Taken → Floor Request (≤ priority, queuing) → queue + Queued 응답
//!   G:Taken → Floor Release → queue pop → G:Taken (next) or G:Idle
//!   G:Taken → T2/ping timeout → revoke → queue pop → ...

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
        /// 현재 발화자 우선순위
        speaker_priority: u8,
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
    Granted {
        speaker: String,
        priority: u8,
        duration_s: u16,
    },
    /// 발화권 거부 — 요청자에게 Deny
    Denied {
        reason: String,
        current_speaker: String,
    },
    /// 큐에 삽입됨 — 요청자에게 Queued 응답 (3GPP Floor Deny + Queue Info)
    Queued {
        user_id: String,
        position: u8,
        priority: u8,
        queue_size: u16,
    },
    /// 발화권 해제 — 전체에 Floor Idle
    Released {
        prev_speaker: String,
    },
    /// 서버 강제 회수 — 발화자에게 Revoke, 전체에 Floor Idle
    Revoked {
        prev_speaker: String,
        cause: String,
    },
    /// Floor PING 정상 응답
    PingOk,
    /// Floor PING 거부 (발화자가 아닌 클라이언트가 보냄)
    PingDenied,
    /// 해당 요청이 무효 (PTT 모드가 아닌 room 등)
    NotPttRoom,
}

// ============================================================================
// FloorConfig — 방별 Floor Control 설정
// ============================================================================

#[derive(Debug, Clone)]
pub struct FloorConfig {
    /// 큐잉 활성화 여부
    pub queuing_enabled: bool,
    /// 최대 큐 크기
    pub max_queue_size: usize,
    /// 선점 활성화 여부
    pub preemption_enabled: bool,
    /// 최대 Talk Burst 시간 (ms)
    pub max_burst_ms: u64,
    /// PING 미수신 타임아웃 (ms)
    pub ping_timeout_ms: u64,
}

impl Default for FloorConfig {
    fn default() -> Self {
        Self {
            queuing_enabled: true,
            max_queue_size: config::FLOOR_QUEUE_MAX_SIZE,
            preemption_enabled: true,
            max_burst_ms: config::FLOOR_MAX_BURST_MS,
            ping_timeout_ms: config::FLOOR_PING_TIMEOUT_MS,
        }
    }
}

// ============================================================================
// QueueEntry — 대기열 항목
// ============================================================================

#[derive(Debug, Clone)]
struct QueueEntry {
    user_id: String,
    priority: u8,
    enqueued_at: u64,
}

// ============================================================================
// FloorInner — Mutex로 보호되는 내부 상태 (원자성 보장)
// ============================================================================

struct FloorInner {
    state: FloorState,
    /// 우선순위 큐: priority DESC → enqueued_at ASC
    queue: Vec<QueueEntry>,
    /// 직전 발화자 (NACK 역매핑 fallback)
    prev_speaker: Option<String>,
}

// ============================================================================
// FloorController
// ============================================================================

/// Room 단위 Floor Control 상태 머신 v2.
/// signaling handler에서 호출하고, 반환된 Vec<FloorAction>에 따라 메시지 전송.
pub struct FloorController {
    inner: Mutex<FloorInner>,
    pub config: FloorConfig,
}

impl FloorController {
    pub fn new() -> Self {
        Self::with_config(FloorConfig::default())
    }

    pub fn with_config(config: FloorConfig) -> Self {
        Self {
            inner: Mutex::new(FloorInner {
                state: FloorState::Idle,
                queue: Vec::new(),
                prev_speaker: None,
            }),
            config,
        }
    }

    /// 현재 Floor 상태 스냅샷
    pub fn current_state(&self) -> FloorState {
        self.inner.lock().unwrap().state.clone()
    }

    /// 현재 발화자 user_id (없으면 None)
    pub fn current_speaker(&self) -> Option<String> {
        match &self.inner.lock().unwrap().state {
            FloorState::Taken { speaker, .. } => Some(speaker.clone()),
            FloorState::Idle => None,
        }
    }

    /// 현재 발화자 우선순위 (없으면 None)
    pub fn current_speaker_priority(&self) -> Option<u8> {
        match &self.inner.lock().unwrap().state {
            FloorState::Taken { speaker_priority, .. } => Some(*speaker_priority),
            FloorState::Idle => None,
        }
    }

    /// 직전 발화자 (Idle 전환 후에도 유효, NACK 역매핑 fallback용)
    pub fn last_speaker(&self) -> Option<String> {
        self.inner.lock().unwrap().prev_speaker.clone()
    }

    /// 큐 스냅샷 — ROOM_SYNC 용 [(user_id, priority, position)]
    pub fn queue_snapshot(&self) -> Vec<(String, u8, u8)> {
        let inner = self.inner.lock().unwrap();
        inner.queue.iter().enumerate().map(|(i, e)| {
            (e.user_id.clone(), e.priority, (i + 1) as u8)
        }).collect()
    }

    /// 큐 크기
    pub fn queue_size(&self) -> usize {
        self.inner.lock().unwrap().queue.len()
    }

    // ========================================================================
    // Floor Request — 발화권 요청 (PTT 누름)
    // ========================================================================

    pub fn request(&self, user_id: &str, priority: u8, now_ms: u64) -> Vec<FloorAction> {
        let mut inner = self.inner.lock().unwrap();
        match &inner.state {
            FloorState::Idle => {
                info!("[FLOOR] granted user={} priority={}", user_id, priority);
                inner.state = FloorState::Taken {
                    speaker: user_id.to_string(),
                    speaker_priority: priority,
                    burst_start: now_ms,
                    last_ping: now_ms,
                };
                inner.prev_speaker = None;
                vec![FloorAction::Granted {
                    speaker: user_id.to_string(),
                    priority,
                    duration_s: config::FLOOR_DEFAULT_DURATION_S,
                }]
            }
            FloorState::Taken { speaker, speaker_priority, .. } => {
                if speaker == user_id {
                    // 이미 발화 중인 사용자가 다시 요청 — idempotent
                    return vec![FloorAction::Granted {
                        speaker: user_id.to_string(),
                        priority: *speaker_priority,
                        duration_s: config::FLOOR_DEFAULT_DURATION_S,
                    }];
                }

                let cur_priority = *speaker_priority;
                let cur_speaker = speaker.clone();

                // 선점 판정: 요청 우선순위 > 현재 발화자 우선순위
                if self.config.preemption_enabled && priority > cur_priority {
                    info!("[FLOOR] preempt prev={} (pri={}) by new={} (pri={})",
                        cur_speaker, cur_priority, user_id, priority);
                    inner.prev_speaker = Some(cur_speaker.clone());
                    inner.state = FloorState::Taken {
                        speaker: user_id.to_string(),
                        speaker_priority: priority,
                        burst_start: now_ms,
                        last_ping: now_ms,
                    };
                    // 선점된 사용자는 큐에 넣지 않음 (3GPP 규격)
                    return vec![
                        FloorAction::Revoked {
                            prev_speaker: cur_speaker,
                            cause: "preempted".to_string(),
                        },
                        FloorAction::Granted {
                            speaker: user_id.to_string(),
                            priority,
                            duration_s: config::FLOOR_DEFAULT_DURATION_S,
                        },
                    ];
                }

                // 큐잉 판정
                if self.config.queuing_enabled {
                    // 이미 큐에 있으면 중복 삽입 방지
                    if inner.queue.iter().any(|e| e.user_id == user_id) {
                        let pos = inner.queue.iter()
                            .position(|e| e.user_id == user_id)
                            .unwrap();
                        return vec![FloorAction::Queued {
                            user_id: user_id.to_string(),
                            position: (pos + 1) as u8,
                            priority,
                            queue_size: inner.queue.len() as u16,
                        }];
                    }

                    // 큐 풀 체크
                    if inner.queue.len() >= self.config.max_queue_size {
                        info!("[FLOOR] denied user={} (queue full, speaker={})",
                            user_id, cur_speaker);
                        return vec![FloorAction::Denied {
                            reason: "queue full".to_string(),
                            current_speaker: cur_speaker,
                        }];
                    }

                    // 큐 삽입
                    inner.queue.push(QueueEntry {
                        user_id: user_id.to_string(),
                        priority,
                        enqueued_at: now_ms,
                    });
                    // 정렬: priority DESC → enqueued_at ASC
                    inner.queue.sort_by(|a, b| {
                        b.priority.cmp(&a.priority)
                            .then(a.enqueued_at.cmp(&b.enqueued_at))
                    });
                    let pos = inner.queue.iter()
                        .position(|e| e.user_id == user_id)
                        .unwrap();
                    info!("[FLOOR] queued user={} priority={} position={}",
                        user_id, priority, pos + 1);
                    vec![FloorAction::Queued {
                        user_id: user_id.to_string(),
                        position: (pos + 1) as u8,
                        priority,
                        queue_size: inner.queue.len() as u16,
                    }]
                } else {
                    // 큐잉 비활성
                    info!("[FLOOR] denied user={} (speaker={})", user_id, cur_speaker);
                    vec![FloorAction::Denied {
                        reason: "channel busy".to_string(),
                        current_speaker: cur_speaker,
                    }]
                }
            }
        }
    }

    // ========================================================================
    // Floor Release — 발화권 자진 해제 (PTT 뗌)
    // ========================================================================

    pub fn release(&self, user_id: &str) -> Vec<FloorAction> {
        let mut inner = self.inner.lock().unwrap();
        match &inner.state {
            FloorState::Taken { speaker, .. } if speaker == user_id => {
                info!("[FLOOR] released user={}", user_id);
                inner.prev_speaker = Some(user_id.to_string());
                inner.state = FloorState::Idle;
                let mut actions = vec![FloorAction::Released {
                    prev_speaker: user_id.to_string(),
                }];
                // 큐에서 다음 발화자 자동 grant
                if let Some(grant) = Self::_try_grant_next(&mut inner) {
                    actions.push(grant);
                }
                actions
            }
            FloorState::Taken { speaker, .. } => {
                // 발화자가 아닌 사용자가 release
                // 큐에 있으면 큐에서 제거 (큐 취소)
                let speaker_name = speaker.clone();
                let before = inner.queue.len();
                inner.queue.retain(|e| e.user_id != user_id);
                if inner.queue.len() < before {
                    info!("[FLOOR] queue cancel user={}", user_id);
                }
                warn!("[FLOOR] release ignored user={} (speaker={})", user_id, speaker_name);
                vec![]
            }
            FloorState::Idle => {
                // 이미 Idle — idempotent
                vec![FloorAction::Released {
                    prev_speaker: user_id.to_string(),
                }]
            }
        }
    }

    // ========================================================================
    // Floor PING — 발화자 생존 확인
    // ========================================================================

    pub fn ping(&self, user_id: &str, now_ms: u64) -> FloorAction {
        let mut inner = self.inner.lock().unwrap();
        match &mut inner.state {
            FloorState::Taken { speaker, last_ping, .. } if speaker == user_id => {
                *last_ping = now_ms;
                FloorAction::PingOk
            }
            _ => FloorAction::PingDenied,
        }
    }

    // ========================================================================
    // 큐 위치 조회
    // ========================================================================

    pub fn query_queue_position(&self, user_id: &str) -> Option<(u8, u8)> {
        let inner = self.inner.lock().unwrap();
        inner.queue.iter().enumerate().find(|(_, e)| e.user_id == user_id)
            .map(|(i, e)| ((i + 1) as u8, e.priority))
    }

    // ========================================================================
    // 큐 취소
    // ========================================================================

    pub fn cancel_queue(&self, user_id: &str) -> bool {
        let mut inner = self.inner.lock().unwrap();
        let before = inner.queue.len();
        inner.queue.retain(|e| e.user_id != user_id);
        inner.queue.len() < before
    }

    // ========================================================================
    // 타이머 체크 — 주기적 호출 (floor timer task)
    // ========================================================================

    pub fn check_timers(&self, now_ms: u64) -> Vec<FloorAction> {
        let mut inner = self.inner.lock().unwrap();
        match &inner.state {
            FloorState::Taken { speaker, burst_start, last_ping, .. } => {
                // T2: max talk burst 초과
                if now_ms.saturating_sub(*burst_start) > self.config.max_burst_ms {
                    let prev = speaker.clone();
                    warn!("[FLOOR] T2 expired user={} burst={}ms",
                        prev, now_ms.saturating_sub(*burst_start));
                    inner.prev_speaker = Some(prev.clone());
                    inner.state = FloorState::Idle;
                    let mut actions = vec![FloorAction::Revoked {
                        prev_speaker: prev,
                        cause: "max burst exceeded".to_string(),
                    }];
                    if let Some(grant) = Self::_try_grant_next(&mut inner) {
                        actions.push(grant);
                    }
                    return actions;
                }
                // T_FLOOR_TIMEOUT: ping 미수신
                if now_ms.saturating_sub(*last_ping) > self.config.ping_timeout_ms {
                    let prev = speaker.clone();
                    warn!("[FLOOR] ping timeout user={} last_ping={}ms ago",
                        prev, now_ms.saturating_sub(*last_ping));
                    inner.prev_speaker = Some(prev.clone());
                    inner.state = FloorState::Idle;
                    let mut actions = vec![FloorAction::Revoked {
                        prev_speaker: prev,
                        cause: "participant lost".to_string(),
                    }];
                    if let Some(grant) = Self::_try_grant_next(&mut inner) {
                        actions.push(grant);
                    }
                    return actions;
                }
                vec![]
            }
            FloorState::Idle => vec![],
        }
    }

    // ========================================================================
    // 참가자 퇴장 — 발화자/큐 제거 + 자동 grant
    // ========================================================================

    pub fn on_participant_leave(&self, user_id: &str) -> Vec<FloorAction> {
        let mut inner = self.inner.lock().unwrap();

        // 큐에서 제거 (발화자든 아니든)
        inner.queue.retain(|e| e.user_id != user_id);

        match &inner.state {
            FloorState::Taken { speaker, .. } if speaker == user_id => {
                info!("[FLOOR] auto-release on leave user={}", user_id);
                inner.prev_speaker = Some(user_id.to_string());
                inner.state = FloorState::Idle;
                let mut actions = vec![FloorAction::Released {
                    prev_speaker: user_id.to_string(),
                }];
                if let Some(grant) = Self::_try_grant_next(&mut inner) {
                    actions.push(grant);
                }
                actions
            }
            _ => vec![],
        }
    }

    // ========================================================================
    // 내부 — 큐에서 다음 발화자 grant
    // ========================================================================

    /// 큐 최상위 항목을 pop하여 Granted 액션 반환.
    /// 호출 시점에 inner.state == Idle이어야 함.
    /// inner lock이 이미 잡혀있는 상태에서 호출 (&mut FloorInner).
    fn _try_grant_next(inner: &mut FloorInner) -> Option<FloorAction> {
        if inner.queue.is_empty() {
            return None;
        }
        let next = inner.queue.remove(0); // 정렬 유지됨, 0번이 최고 우선순위
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        info!("[FLOOR] queue pop → granted user={} priority={} remaining={}",
            next.user_id, next.priority, inner.queue.len());
        inner.state = FloorState::Taken {
            speaker: next.user_id.clone(),
            speaker_priority: next.priority,
            burst_start: now,
            last_ping: now,
        };
        inner.prev_speaker = None;
        Some(FloorAction::Granted {
            speaker: next.user_id,
            priority: next.priority,
            duration_s: config::FLOOR_DEFAULT_DURATION_S,
        })
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> u64 {
        1000000
    }

    #[test]
    fn test_basic_grant_release() {
        let fc = FloorController::new();
        let actions = fc.request("A", 0, now());
        assert_eq!(actions.len(), 1);
        assert!(matches!(&actions[0], FloorAction::Granted { speaker, .. } if speaker == "A"));

        let actions = fc.release("A");
        assert_eq!(actions.len(), 1);
        assert!(matches!(&actions[0], FloorAction::Released { .. }));
    }

    #[test]
    fn test_preemption() {
        let fc = FloorController::new();
        fc.request("A", 2, now());

        let actions = fc.request("B", 5, now());
        assert_eq!(actions.len(), 2);
        assert!(matches!(&actions[0], FloorAction::Revoked { prev_speaker, cause }
            if prev_speaker == "A" && cause == "preempted"));
        assert!(matches!(&actions[1], FloorAction::Granted { speaker, priority, .. }
            if speaker == "B" && *priority == 5));
    }

    #[test]
    fn test_no_preemption_same_priority() {
        let fc = FloorController::new();
        fc.request("A", 3, now());

        let actions = fc.request("B", 3, now());
        assert_eq!(actions.len(), 1);
        assert!(matches!(&actions[0], FloorAction::Queued { .. }));
    }

    #[test]
    fn test_queue_fifo_same_priority() {
        let fc = FloorController::new();
        fc.request("A", 5, now());         // A가 최고 우선순위로 발화
        fc.request("B", 2, now());         // 2 < 5 → 큐 삽입
        fc.request("C", 2, now() + 1);    // 2 < 5 → 큐 삽입 (B보다 늦게)

        // Release A → B should be granted (same priority as C, but earlier)
        let actions = fc.release("A");
        assert!(actions.len() >= 2);
        assert!(matches!(&actions[1], FloorAction::Granted { speaker, .. } if speaker == "B"));
    }

    #[test]
    fn test_queue_priority_order() {
        let fc = FloorController::new();
        fc.request("A", 10, now());        // A가 최고 우선순위로 발화
        fc.request("B", 1, now());         // 1 < 10 → 큐
        fc.request("C", 5, now());         // 5 < 10 → 큐
        fc.request("D", 3, now());         // 3 < 10 → 큐

        // Release A → C (priority 5, 큐 내 최고) should be granted
        let actions = fc.release("A");
        assert!(actions.len() >= 2);
        assert!(matches!(&actions[1], FloorAction::Granted { speaker, .. } if speaker == "C"));
    }

    #[test]
    fn test_queue_full_deny() {
        let config = FloorConfig {
            max_queue_size: 2,
            ..FloorConfig::default()
        };
        let fc = FloorController::with_config(config);
        fc.request("A", 0, now());
        fc.request("B", 0, now());
        fc.request("C", 0, now());

        let actions = fc.request("D", 0, now());
        assert_eq!(actions.len(), 1);
        assert!(matches!(&actions[0], FloorAction::Denied { reason, .. } if reason == "queue full"));
    }

    #[test]
    fn test_queue_disabled_deny() {
        let config = FloorConfig {
            queuing_enabled: false,
            ..FloorConfig::default()
        };
        let fc = FloorController::with_config(config);
        fc.request("A", 0, now());

        let actions = fc.request("B", 0, now());
        assert_eq!(actions.len(), 1);
        assert!(matches!(&actions[0], FloorAction::Denied { reason, .. } if reason == "channel busy"));
    }

    #[test]
    fn test_participant_leave_speaker_queue_pop() {
        let fc = FloorController::new();
        fc.request("A", 5, now());         // A 발화
        fc.request("B", 2, now());         // 2 < 5 → 큐

        let actions = fc.on_participant_leave("A");
        assert!(actions.len() >= 2);
        assert!(matches!(&actions[0], FloorAction::Released { prev_speaker } if prev_speaker == "A"));
        assert!(matches!(&actions[1], FloorAction::Granted { speaker, .. } if speaker == "B"));
    }

    #[test]
    fn test_participant_leave_queued() {
        let fc = FloorController::new();
        fc.request("A", 5, now());         // A 발화
        fc.request("B", 2, now());         // 2 < 5 → 큐

        let actions = fc.on_participant_leave("B");
        assert!(actions.is_empty());       // B는 큐에서 제거만, 액션 없음
        assert_eq!(fc.queue_size(), 0);
    }

    #[test]
    fn test_idempotent_request() {
        let fc = FloorController::new();
        fc.request("A", 3, now());

        let actions = fc.request("A", 3, now());
        assert_eq!(actions.len(), 1);
        assert!(matches!(&actions[0], FloorAction::Granted { speaker, .. } if speaker == "A"));
    }

    #[test]
    fn test_duplicate_queue() {
        let fc = FloorController::new();
        fc.request("A", 5, now());         // A 발화
        fc.request("B", 2, now());         // 2 < 5 → 큐

        // B가 다시 요청하면 기존 큐 위치 반환
        let actions = fc.request("B", 2, now());
        assert_eq!(actions.len(), 1);
        assert!(matches!(&actions[0], FloorAction::Queued { position, .. } if *position == 1));
        assert_eq!(fc.queue_size(), 1); // 중복 삽입 안 됨
    }

    #[test]
    fn test_check_timers_revoke_queue_pop() {
        let config = FloorConfig {
            max_burst_ms: 100,
            ..FloorConfig::default()
        };
        let fc = FloorController::with_config(config);
        fc.request("A", 5, 1000);          // A 발화 (pri=5)
        fc.request("B", 2, 1000);          // 2 < 5 → 큐

        // 200ms 후 → T2 만료 → A revoke → B grant
        let actions = fc.check_timers(1200);
        assert_eq!(actions.len(), 2);
        assert!(matches!(&actions[0], FloorAction::Revoked { prev_speaker, .. } if prev_speaker == "A"));
        assert!(matches!(&actions[1], FloorAction::Granted { speaker, .. } if speaker == "B"));
    }
}
