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

use std::fmt;
use std::net::SocketAddr;
use std::sync::Arc;
use axum::Router;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use tracing_subscriber::fmt::time::FormatTime;

use crate::signaling::message::*;
use crate::signaling::opcode;
use crate::state::AppState;
use crate::signaling::handler;
use crate::transport::dtls::ServerCert;
use crate::metrics::GlobalMetrics;
use crate::transport::udp::UdpTransport;

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
        state.admin_tx.clone(),
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
                state.admin_tx.clone(),
                i as u8,
                bwe_mode,
                remb_bitrate,
                Arc::clone(&metrics),
            );
            tokio::spawn(async move { w.run().await; });
        }
    }

    // Start zombie reaper
    let reaper_cancel = cancel.clone();
    let reaper_rooms = Arc::clone(&state.rooms);
    tokio::spawn(async move {
        run_zombie_reaper(reaper_rooms, reaper_cancel).await;
    });

    // Start floor timer (PTT T2/T_FLOOR_TIMEOUT 감시)
    let floor_cancel = cancel.clone();
    let floor_rooms = Arc::clone(&state.rooms);
    let floor_metrics = Arc::clone(&metrics);
    tokio::spawn(async move {
        run_floor_timer(floor_rooms, floor_metrics, floor_cancel).await;
    });

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

// ============================================================================
// .env 파일 탐색 + 헬퍼
// ============================================================================

/// .env 파일 탐색 및 로드
///
/// 우선순위:
///   1. `--env /path/to/.env` CLI 인자 → 해당 경로 (없으면 에러 출력 후 무시)
///   2. CWD/.env
///   3. 실행파일 디렉토리/.env
///   4. 모두 없으면 환경변수/기본값으로 동작
fn load_env_file() {
    let args: Vec<String> = std::env::args().collect();

    // --env /path 인자 처리
    if let Some(pos) = args.iter().position(|a| a == "--env") {
        if let Some(path) = args.get(pos + 1) {
            match dotenvy::from_path(std::path::Path::new(path)) {
                Ok(_) => {
                    eprintln!("[env] loaded: {}", path);
                    return;
                }
                Err(e) => {
                    eprintln!("[env] WARN: --env {} failed: {}", path, e);
                    // fallback으로 계속
                }
            }
        } else {
            eprintln!("[env] WARN: --env requires a path argument");
        }
    }

    // CWD/.env
    if dotenvy::dotenv().is_ok() {
        eprintln!("[env] loaded: .env (CWD)");
        return;
    }

    // 실행파일 디렉토리/.env
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let env_path = dir.join(".env");
            if env_path.is_file() {
                match dotenvy::from_path(&env_path) {
                    Ok(_) => {
                        eprintln!("[env] loaded: {}", env_path.display());
                        return;
                    }
                    Err(e) => {
                        eprintln!("[env] WARN: {} failed: {}", env_path.display(), e);
                    }
                }
            }
        }
    }

    eprintln!("[env] no .env file found, using environment variables / defaults");
}

/// 환경변수에서 값 로드, 실패 시 기본값 반환
fn env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// 라우팅 테이블 기반 로컬 IP 감지 (PUBLIC_IP 미설정 시 fallback)
fn detect_local_ip() -> String {
    std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| {
            s.connect("8.8.8.8:80")?;
            s.local_addr()
        })
        .map(|addr| addr.ip().to_string())
        .unwrap_or_else(|_| "127.0.0.1".to_string())
}

/// 서버 기동 시 기본 방 생성 (테스트/개발용)
fn create_default_rooms(state: &AppState) {
    let defaults: [(&str, usize, config::RoomMode); 3] = [
        ("무전 대화방", 10, config::RoomMode::Ptt),
        ("회의실-2", 10, config::RoomMode::Conference),
        ("대회의실", 20, config::RoomMode::Conference),
    ];

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    for (name, capacity, mode) in defaults {
        let room = state.rooms.create(name.to_string(), Some(capacity), mode, now);
        info!("default room created: {} (id={}, cap={}, mode={})", name, room.id, capacity, room.mode);
    }
}

/// Floor Control 타이머 태스크 (PTT T2/T_FLOOR_TIMEOUT 감시)
/// 2초 주기로 PTT 모드 room의 floor 타이머를 체크하여
/// max burst 초과 또는 ping 미수신 시 발화권을 강제 회수한다.
async fn run_floor_timer(
    rooms: Arc<crate::room::room::RoomHub>,
    metrics: Arc<crate::metrics::GlobalMetrics>,
    cancel: CancellationToken,
) {
    let mut timer = tokio::time::interval(
        tokio::time::Duration::from_millis(config::FLOOR_PING_INTERVAL_MS),
    );
    timer.tick().await; // 첫 tick 즉시 소비

    loop {
        tokio::select! {
            _ = timer.tick() => {}
            _ = cancel.cancelled() => {
                info!("floor timer stopped (shutdown)");
                return;
            }
        }

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        // PTT 모드 room만 순회
        for entry in rooms.rooms.iter() {
            let room = entry.value();
            if room.mode != config::RoomMode::Ptt {
                continue;
            }

            if let Some(action) = room.floor.check_timers(now) {
                match &action {
                    crate::room::floor::FloorAction::Revoked { prev_speaker, cause } => {
                        metrics.ptt_floor_revoked.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        warn!("[FLOOR] timer revoke: user={} cause={} room={}",
                            prev_speaker, cause, room.id);

                        // PLI burst cancel + rewriter 정리
                        if let Some(p) = room.get_participant(prev_speaker) {
                            p.cancel_pli_burst();
                        }
                        room.audio_rewriter.clear_speaker();
                        room.video_rewriter.clear_speaker();

                        // 발화자에게 FLOOR_REVOKE
                        if let Some(p) = room.get_participant(prev_speaker) {
                            let revoke = Packet::new(
                                opcode::FLOOR_REVOKE,
                                0,
                                serde_json::json!({
                                    "room_id": &room.id,
                                    "cause": cause,
                                }),
                            );
                            let json = serde_json::to_string(&revoke).unwrap_or_default();
                            let _ = p.ws_tx.send(json);
                        }

                        // 전체에 FLOOR_IDLE
                        let idle = Packet::new(
                            opcode::FLOOR_IDLE,
                            0,
                            serde_json::json!({
                                "room_id": &room.id,
                                "prev_speaker": prev_speaker,
                            }),
                        );
                        broadcast_to_room_all(&room, &idle);
                    }
                    _ => {} // check_timers는 Revoked만 반환
                }
            }
        }
    }
}

/// 좀비 세션 주기적 정리 태스크
/// last_seen + ZOMBIE_TIMEOUT_MS < now 인 participant를 room에서 제거하고
/// 나머지 참가자들에게 leave/tracks_update 브로드캐스트
async fn run_zombie_reaper(
    rooms: Arc<crate::room::room::RoomHub>,
    cancel: CancellationToken,
) {
    let mut timer = tokio::time::interval(
        tokio::time::Duration::from_millis(config::REAPER_INTERVAL_MS),
    );
    timer.tick().await; // 첫 tick 즉시 소비

    loop {
        tokio::select! {
            _ = timer.tick() => {}
            _ = cancel.cancelled() => {
                info!("zombie reaper stopped (shutdown)");
                return;
            }
        }

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        let reaped = rooms.reap_zombies(now, config::ZOMBIE_TIMEOUT_MS);

        for (room_id, zombie) in &reaped {
            // PLI burst cancel (좀비가 발화자였을 수 있음)
            zombie.cancel_pli_burst();

            warn!("zombie reaped: user={} room={} last_seen={}ms ago",
                zombie.user_id, room_id,
                now.saturating_sub(zombie.last_seen.load(std::sync::atomic::Ordering::Relaxed)));

            // 남은 참가자에게 tracks_update(remove) + participant_left 브로드캐스트
            if let Ok(room) = rooms.get(&room_id) {
                let tracks = zombie.get_tracks();
                if !tracks.is_empty() {
                    let remove_tracks: Vec<serde_json::Value> = tracks.iter().map(|t| {
                        let mut j = serde_json::json!({
                            "user_id": &zombie.user_id,
                            "kind": t.kind.to_string(),
                            "ssrc": t.ssrc,
                            "track_id": &t.track_id,
                        });
                        if let Some(rs) = t.rtx_ssrc {
                            j["rtx_ssrc"] = serde_json::json!(rs);
                        }
                        j
                    }).collect();
                    let tracks_event = Packet::new(
                        opcode::TRACKS_UPDATE,
                        0,
                        serde_json::json!({
                            "action": "remove",
                            "tracks": remove_tracks,
                        }),
                    );
                    broadcast_to_room_all(&room, &tracks_event);
                }

                let leave_event = Packet::new(
                    opcode::ROOM_EVENT,
                    0,
                    serde_json::json!({
                        "type": "participant_left",
                        "room_id": room_id,
                        "user_id": &zombie.user_id,
                    }),
                );
                broadcast_to_room_all(&room, &leave_event);
            }
        }

        if !reaped.is_empty() {
            debug!("reaper cycle: {} zombies removed", reaped.len());
        }
    }
}

/// reaper용 브로드캐스트 (room 내 모든 참가자에게 전송)
fn broadcast_to_room_all(room: &crate::room::room::Room, packet: &Packet) {
    let json = match serde_json::to_string(packet) {
        Ok(j) => j,
        Err(_) => return,
    };
    for entry in room.participants.iter() {
        let _ = entry.value().ws_tx.send(json.clone());
    }
}
