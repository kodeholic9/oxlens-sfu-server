// author: kodeholic (powered by Claude)
//! TelemetryBus — 수집/전송 책임 분리를 위한 중앙 텔레메트리 허브
//!
//! 수집 측은 `telemetry_bus::emit()` 하나만 알면 됨.
//! AppState, admin_tx, UdpTransport 등 외부 의존성 zero.
//!
//! 구조:
//!   OnceLock<TelemetryBus>
//!     ├── tx: mpsc::Sender<TelemetryEvent>      — emit() 진입점
//!     └── admin_tx: broadcast::Sender<String>    — subscribe() 용
//!
//!   Bus 태스크 (별도 tokio task):
//!     mpsc::Receiver → 가공 → broadcast::Sender → Admin WS
//!                                                → (미래) 파일/AI

use std::sync::OnceLock;
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, warn};

/// Admin broadcast channel capacity
const ADMIN_CHANNEL_SIZE: usize = 256;
/// mpsc 버퍼 크기 (수집 측 backpressure 방지)
const BUS_CHANNEL_SIZE: usize = 512;

// ============================================================================
// TelemetryEvent — 수집 데이터 종류
// ============================================================================

/// 수집 측이 버스에 보내는 이벤트 종류.
/// 새 메트릭 항목 추가 시 variant 하나 추가하면 끝.
pub enum TelemetryEvent {
    /// 클라이언트 telemetry passthrough (op=30 → 어드민)
    ClientTelemetry {
        user_id: String,
        room_id: String,
        data: serde_json::Value,
    },
    /// 서버 메트릭 (3초 flush — GlobalMetrics + PipelineStats)
    ServerMetrics(serde_json::Value),
    /// Room 상태 스냅샷 (join/leave/create 시 어드민 push)
    RoomSnapshot(serde_json::Value),
    /// AggLogger flush (3초 주기 집계 로그)
    AggLog(serde_json::Value),
}

// ============================================================================
// TelemetryBus — 전역 싱글톤
// ============================================================================

#[derive(Debug)]
pub struct TelemetryBus {
    /// 수집 측 → bus 태스크 (mpsc)
    tx: mpsc::Sender<TelemetryEvent>,
    /// 어드민 WS 구독용 (broadcast)
    admin_tx: broadcast::Sender<String>,
}

static BUS: OnceLock<TelemetryBus> = OnceLock::new();

/// 서버 시작 시 1회 호출. bus 태스크를 spawn하고 전역 싱글톤 등록.
/// 반환값: 없음 (OnceLock에 저장)
pub fn init() {
    let (tx, rx) = mpsc::channel(BUS_CHANNEL_SIZE);
    let (admin_tx, _) = broadcast::channel(ADMIN_CHANNEL_SIZE);

    let bus = TelemetryBus {
        tx,
        admin_tx: admin_tx.clone(),
    };

    // Bus 태스크: mpsc에서 수신 → broadcast로 전달
    let admin_tx_clone = admin_tx;
    tokio::spawn(async move {
        run_bus_task(rx, admin_tx_clone).await;
    });

    BUS.set(bus).expect("TelemetryBus already initialized");
    debug!("[TBUS] initialized");
}

/// 어드민 WS 구독용 — admin.rs에서 사용
pub fn subscribe() -> Option<broadcast::Receiver<String>> {
    BUS.get().map(|bus| bus.admin_tx.subscribe())
}

/// 수집 측 진입점 — 어디서든 호출 가능, 의존성 zero.
/// 채널 full이면 drop (핫패스 안 막힘).
pub fn emit(event: TelemetryEvent) {
    if let Some(bus) = BUS.get() {
        if bus.tx.try_send(event).is_err() {
            // 채널 포화 시 drop — 로깅 데이터 유실은 허용
            debug!("[TBUS] channel full, event dropped");
        }
    }
}

// ============================================================================
// Bus 태스크 — mpsc 수신 → 가공 → broadcast 전달
// ============================================================================

async fn run_bus_task(
    mut rx: mpsc::Receiver<TelemetryEvent>,
    admin_tx: broadcast::Sender<String>,
) {
    debug!("[TBUS] bus task started");

    while let Some(event) = rx.recv().await {
        let json_str = match event {
            TelemetryEvent::ClientTelemetry { user_id, room_id, data } => {
                serde_json::json!({
                    "type": "client_telemetry",
                    "user_id": user_id,
                    "room_id": room_id,
                    "data": data,
                }).to_string()
            }
            TelemetryEvent::ServerMetrics(json) => {
                json.to_string()
            }
            TelemetryEvent::RoomSnapshot(json) => {
                json.to_string()
            }
            TelemetryEvent::AggLog(json) => {
                json.to_string()
            }
        };

        // broadcast — receiver 없으면 에러지만 무시
        if admin_tx.send(json_str).is_err() {
            // 어드민 미접속 시 정상 — 로그 불필요
        }
    }

    warn!("[TBUS] bus task ended (all senders dropped)");
}
