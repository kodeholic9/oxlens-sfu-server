// author: kodeholic (powered by Claude)
//! Global application state shared across handlers

use std::sync::Arc;

use tokio::net::UdpSocket;

use crate::config::BweMode;
use crate::metrics::GlobalMetrics;
use crate::room::room::RoomHub;
use crate::transport::dtls::ServerCert;

/// Shared application state (passed to all handlers via Axum's State extractor)
#[derive(Clone)]
pub struct AppState {
    pub rooms:      Arc<RoomHub>,
    pub cert:       Arc<ServerCert>,
    pub udp_socket: Arc<UdpSocket>,
    /// Global metrics (PTT 카운터 등 handler에서도 접근)
    pub(crate) metrics: Arc<GlobalMetrics>,
    /// ICE candidate에 노출할 공인 IP (.env PUBLIC_IP 또는 자동 감지)
    pub public_ip:  String,
    /// WebSocket 시그널링 포트
    pub ws_port:    u16,
    /// UDP 미디어 포트
    pub udp_port:   u16,
    /// BWE 모드 (TWCC or REMB, .env BWE_MODE)
    pub bwe_mode:   BweMode,
    /// REMB 비트레이트 (bps, .env REMB_BITRATE_BPS)
    pub remb_bitrate: u64,
}

impl AppState {
    pub(crate) fn new(
        cert: ServerCert,
        udp_socket: Arc<UdpSocket>,
        public_ip: String,
        ws_port: u16,
        udp_port: u16,
        bwe_mode: BweMode,
        remb_bitrate: u64,
        metrics: Arc<GlobalMetrics>,
    ) -> Self {
        Self {
            rooms:      Arc::new(RoomHub::new()),
            cert:       Arc::new(cert),
            udp_socket,
            metrics,
            public_ip,
            ws_port,
            udp_port,
            bwe_mode,
            remb_bitrate,
        }
    }
}
