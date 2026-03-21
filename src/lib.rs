// author: kodeholic (powered by Claude)
#![recursion_limit = "256"]
//! oxlens-sfu-server: High-performance SFU server with Bundle + ICE-Lite

pub mod config;
pub mod error;
pub mod metrics;
pub mod signaling;
pub mod transport;
pub mod media;
pub mod room;
pub mod state;
pub mod tasks;
pub mod startup;
pub mod telemetry_bus;
pub mod agg_logger;

use std::fmt;
use std::net::SocketAddr;
use std::sync::Arc;
use axum::Router;
use tokio_util::sync::CancellationToken;
use tracing::info;
use tracing_subscriber::fmt::time::FormatTime;

use crate::state::AppState;
use crate::signaling::handler;
use crate::transport::dtls::ServerCert;
use crate::metrics::GlobalMetrics;
use crate::transport::udp::UdpTransport;
use crate::startup::{load_env_file, env_or, detect_local_ip, create_default_rooms};
use crate::tasks::{run_floor_timer, run_zombie_reaper};

// ============================================================================
// Local time formatter for tracing
// ============================================================================

struct LocalTimer;

impl FormatTime for LocalTimer {
    fn format_time(&self, w: &mut tracing_subscriber::fmt::format::Writer<'_>) -> fmt::Result {
        let now = chrono::Local::now();
        write!(w, "{}", now.format("%Y-%m-%d %H:%M:%S%.3f"))
    }
}

/// Run the SFU server
pub async fn run_server() -> Result<(), Box<dyn std::error::Error>> {
    // ========================================================================
    // 1. .env 파일 탐색
    //    --env /path  → 해당 경로 (없으면 에러)
    //    인자 없음    → CWD/.env → 실행파일 디렉토리/.env → 둘 다 없으면 환경변수/기본값
    // ========================================================================
    load_env_file();

    // ========================================================================
    // 2. 설정값 로드 (fallback: config 상수 / 자동 감지)
    // ========================================================================
    let ws_port: u16 = env_or("WS_PORT", config::WS_PORT);
    let udp_port: u16 = env_or("UDP_PORT", config::UDP_PORT);
    let _udp_workers: usize = env_or("UDP_WORKER_COUNT", config::UDP_WORKER_COUNT);
    let public_ip: String = std::env::var("PUBLIC_IP")
        .unwrap_or_else(|_| detect_local_ip());
    let bwe_mode = config::resolve_bwe_mode();
    let remb_bitrate = config::resolve_remb_bitrate();
    let log_dir: Option<String> = std::env::var("LOG_DIR").ok()
        .filter(|d| !d.is_empty() && std::path::Path::new(d).is_dir());
    let log_level: String = std::env::var("LOG_LEVEL")
        .unwrap_or_else(|_| "info".to_string());

    // ========================================================================
    // 3. tracing 초기화 (LOG_DIR 있으면 일별 로테이션 파일 + 콘솔, 없으면 콘솔만)
    // ========================================================================
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| format!("oxlens_sfu_server={}", log_level).into());

    if let Some(ref dir) = log_dir {
        // 일별 로테이션 파일 (oxsfud.YYYY-MM-DD.log)
        let file_appender = tracing_appender::rolling::daily(dir, "oxsfud.log");
        let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);
        // _guard를 들고 있어야 flush됨 — Box::leak으로 프로세스 수명 동안 유지
        Box::leak(Box::new(_guard));

        tracing_subscriber::fmt()
            .with_env_filter(env_filter)
            .with_timer(LocalTimer)
            .with_file(true)
            .with_line_number(true)
            .with_target(false)
            .with_writer(non_blocking)
            .with_ansi(false) // 파일에는 ANSI 색상 코드 제거
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(env_filter)
            .with_timer(LocalTimer)
            .with_file(true)
            .with_line_number(true)
            .with_target(false)
            .init();
    }

    info!("config: PUBLIC_IP={} WS_PORT={} UDP_PORT={} BWE_MODE={} REMB_BPS={} LOG_DIR={}",
        public_ip, ws_port, udp_port, bwe_mode, remb_bitrate, log_dir.as_deref().unwrap_or("(stdout)"));

    // Generate DTLS server certificate (once per instance)
    let cert = ServerCert::generate()?;
    info!("DTLS fingerprint: {}", cert.fingerprint);

    // ========================================================================
    // Phase W-2: UDP multi-worker (Linux SO_REUSEPORT) / single worker (Windows)
    // ========================================================================
    #[allow(unused_variables)]
    let (primary_socket, worker_count) = {
        #[cfg(target_os = "linux")]
        {
            use crate::transport::udp::{resolve_worker_count, bind_reuseport};
            let count = resolve_worker_count(_udp_workers);
            let sock = Arc::new(bind_reuseport(udp_port)?);
            info!("UDP SO_REUSEPORT: {} workers on port {}", count, udp_port);
            (sock, count)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let addr = std::net::SocketAddr::from(([0, 0, 0, 0], udp_port));
            let sock = Arc::new(tokio::net::UdpSocket::bind(addr).await?);
            info!("UDP single worker on port {}", udp_port);
            (sock, 1usize)
        }
    };

    // Phase GM: 전역 메트릭 (모든 worker + egress task + handler 공유)
    let metrics = Arc::new(GlobalMetrics::new(worker_count, &bwe_mode));

    // TelemetryBus 초기화 (AppState 생성 전 — 수집/전송 책임 분리)
    telemetry_bus::init();
    agg_logger::init();

    // Build shared state (primary_socket을 AppState에 공유 — PLI/REMB 전송용)
    let state = AppState::new(cert, Arc::clone(&primary_socket), public_ip, ws_port, udp_port, bwe_mode, remb_bitrate, Arc::clone(&metrics));

    // Create default rooms
    create_default_rooms(&state);

    // Cancellation token for graceful shutdown
    let cancel = CancellationToken::new();

    // Worker-0: primary socket 사용
    let w0 = UdpTransport::from_socket_with_id(
        primary_socket,
        Arc::clone(&state.rooms),
        Arc::clone(&state.cert),
        0,
        bwe_mode,
        remb_bitrate,
        Arc::clone(&metrics),
    );
    tokio::spawn(async move { w0.run().await; });

    // Worker-1..N: SO_REUSEPORT 추가 소켓 (Linux only)
    #[cfg(target_os = "linux")]
    {
        use crate::transport::udp::bind_reuseport;
        for i in 1..worker_count {
            let socket = Arc::new(bind_reuseport(udp_port)?);
            let w = UdpTransport::from_socket_with_id(
                socket,
                Arc::clone(&state.rooms),
                Arc::clone(&state.cert),
                i as u8,
                bwe_mode,
                remb_bitrate,
                Arc::clone(&metrics),
            );
            tokio::spawn(async move { w.run().await; });
        }
    }

    // Start zombie reaper
    {
        let rooms = Arc::clone(&state.rooms);
        let cancel = cancel.clone();
        tokio::spawn(async move { run_zombie_reaper(rooms, cancel).await; });
    }

    // Start floor timer (PTT T2/T_FLOOR_TIMEOUT 감시)
    {
        let rooms = Arc::clone(&state.rooms);
        let metrics = Arc::clone(&metrics);
        let udp_socket = Arc::clone(&state.udp_socket);
        let cancel = cancel.clone();
        tokio::spawn(async move { run_floor_timer(rooms, metrics, udp_socket, cancel).await; });
    }

    // Start WebSocket signaling
    let app = Router::new()
        .route("/ws", axum::routing::get(handler::ws_handler))
        .route("/admin/ws", axum::routing::get(handler::admin_ws_handler))
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], ws_port));
    info!("oxlens-sfu-server v{} listening on {}", env!("CARGO_PKG_VERSION"), addr);

    let listener = tokio::net::TcpListener::bind(addr).await?;

    // Graceful shutdown: Ctrl+C → cancel token → drain
    let shutdown_cancel = cancel.clone();
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            tokio::signal::ctrl_c().await.ok();
            info!("shutdown signal received, draining connections...");
            shutdown_cancel.cancel();
            tokio::time::sleep(tokio::time::Duration::from_millis(
                config::SHUTDOWN_DRAIN_MS,
            )).await;
            info!("drain complete, shutting down");
        })
        .await?;

    Ok(())
}
