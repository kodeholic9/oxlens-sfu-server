// author: kodeholic (powered by Claude)
//! light-livechat: High-performance SFU server with Bundle + ICE-Lite

pub mod config;
pub mod error;
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
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "light_livechat=info".into()),
        )
        .with_timer(LocalTimer)
        .with_file(true)
        .with_line_number(true)
        .with_target(false)
        .init();

    // Generate DTLS server certificate (once per instance)
    let cert = ServerCert::generate()?;
    info!("DTLS fingerprint: {}", cert.fingerprint);

    // Start UDP transport (socket 먼저 생성 → AppState에 공유)
    let udp_socket = {
        let addr = std::net::SocketAddr::from(([0, 0, 0, 0], config::UDP_PORT));
        Arc::new(tokio::net::UdpSocket::bind(addr).await?)
    };
    info!("UDP listening on port {}", config::UDP_PORT);

    // Build shared state (udp_socket 포함)
    let state = AppState::new(cert, Arc::clone(&udp_socket));

    // Create default rooms
    create_default_rooms(&state);

    // Cancellation token for graceful shutdown
    let cancel = CancellationToken::new();

    // UDP transport (기존 socket 재사용)
    let udp = UdpTransport::from_socket(
        udp_socket,
        Arc::clone(&state.rooms),
        Arc::clone(&state.cert),
    );

    tokio::spawn(async move {
        udp.run().await;
    });

    // Start zombie reaper
    let reaper_cancel = cancel.clone();
    let reaper_rooms = Arc::clone(&state.rooms);
    tokio::spawn(async move {
        run_zombie_reaper(reaper_rooms, reaper_cancel).await;
    });

    // Start WebSocket signaling
    let app = Router::new()
        .route("/ws", axum::routing::get(handler::ws_handler))
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], config::WS_PORT));
    info!("light-livechat v{} listening on {}", env!("CARGO_PKG_VERSION"), addr);

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

/// 서버 기동 시 기본 방 생성 (테스트/개발용)
fn create_default_rooms(state: &AppState) {
    let defaults = [
        ("회의실-1", 10),
        ("회의실-2", 10),
        ("대회의실", 20),
    ];

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    for (name, capacity) in defaults {
        let room = state.rooms.create(name.to_string(), Some(capacity), now);
        info!("default room created: {} (id={}, cap={})", name, room.id, capacity);
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
