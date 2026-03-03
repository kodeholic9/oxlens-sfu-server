// author: kodeholic (powered by Claude)
//! light-livechat: High-performance SFU server with Bundle + ICE-Lite

pub mod config;
pub mod error;
pub mod signaling;
pub mod transport;
pub mod media;
pub mod room;
pub mod state;

use std::net::SocketAddr;
use std::sync::Arc;
use axum::Router;
use tracing::info;

use crate::state::AppState;
use crate::signaling::handler;
use crate::transport::dtls::ServerCert;
use crate::transport::udp::UdpTransport;

/// Run the SFU server
pub async fn run_server() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "light_livechat=debug".into()),
        )
        .init();

    // Generate DTLS server certificate (once per instance)
    let cert = ServerCert::generate()?;
    info!("DTLS fingerprint: {}", cert.fingerprint);

    // Build shared state
    let state = AppState::new(cert);

    // Create default rooms
    create_default_rooms(&state);

    // Start UDP transport (shares RoomHub + ServerCert with signaling)
    let udp = UdpTransport::bind(
        Arc::clone(&state.rooms),
        Arc::clone(&state.cert),
    ).await?;
    info!("UDP listening on port {}", config::UDP_PORT);

    tokio::spawn(async move {
        udp.run().await;
    });

    // Start WebSocket signaling
    let app = Router::new()
        .route("/ws", axum::routing::get(handler::ws_handler))
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], config::WS_PORT));
    info!("light-livechat v{} listening on {}", env!("CARGO_PKG_VERSION"), addr);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

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
