// author: kodeholic (powered by Claude)
//! Global application state shared across handlers

use std::sync::Arc;

use tokio::net::UdpSocket;
use tokio::sync::broadcast;

use crate::room::room::RoomHub;
use crate::transport::dtls::ServerCert;

/// Admin broadcast channel capacity
const ADMIN_CHANNEL_SIZE: usize = 256;

/// Shared application state (passed to all handlers via Axum's State extractor)
#[derive(Clone)]
pub struct AppState {
    pub rooms:      Arc<RoomHub>,
    pub cert:       Arc<ServerCert>,
    pub udp_socket: Arc<UdpSocket>,
    /// Admin telemetry broadcast (sender side, receivers subscribe via .subscribe())
    pub admin_tx:   broadcast::Sender<String>,
}

impl AppState {
    pub fn new(cert: ServerCert, udp_socket: Arc<UdpSocket>) -> Self {
        let (admin_tx, _) = broadcast::channel(ADMIN_CHANNEL_SIZE);
        Self {
            rooms:      Arc::new(RoomHub::new()),
            cert:       Arc::new(cert),
            udp_socket,
            admin_tx,
        }
    }
}
