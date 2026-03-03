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

    // Start UDP transport (ICE/DTLS/SRTP)
    let udp = UdpTransport::bind().await?;
    info!(
        "ICE credentials: ufrag={}, pwd={}",
        udp.ice_creds.ufrag, udp.ice_creds.pwd
    );

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
