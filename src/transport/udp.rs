// author: kodeholic (powered by Claude)
//! UDP listener — single port, demux dispatch
//!
//! All media traffic arrives on one UDP port.
//! Packets are classified by first byte (RFC 5764) and dispatched:
//!   STUN → ICE-Lite handler
//!   DTLS → DemuxConn channel → DTLSConn
//!   SRTP → Media pipeline (TODO)

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, error, info, trace, warn};

use crate::config;
use crate::transport::demux::{self, PacketType};

use crate::transport::ice::{self, IceCredentials, IceResult};

/// Per-peer channel for DTLS packet forwarding
pub struct PeerChannel {
    pub tx: mpsc::Sender<Vec<u8>>,
}

/// Shared state for the UDP listener
pub struct UdpTransport {
    pub socket: Arc<UdpSocket>,
    pub ice_creds: IceCredentials,
    /// Map remote address → DTLS channel (populated after ICE succeeds)
    peers: Mutex<HashMap<SocketAddr, PeerChannel>>,
}

impl UdpTransport {
    pub async fn bind() -> std::io::Result<Self> {
        let addr = SocketAddr::from(([0, 0, 0, 0], config::UDP_PORT));
        let socket = UdpSocket::bind(addr).await?;
        let ice_creds = IceCredentials::new();

        info!(
            "UDP transport bound on {}, ICE ufrag={}",
            addr, ice_creds.ufrag
        );

        Ok(Self {
            socket: Arc::new(socket),
            ice_creds,
            peers: Mutex::new(HashMap::new()),
        })
    }

    /// Register a peer's DTLS channel (called after ICE USE-CANDIDATE)
    pub async fn register_peer(
        &self,
        remote: SocketAddr,
        tx: mpsc::Sender<Vec<u8>>,
    ) {
        let mut peers = self.peers.lock().await;
        peers.insert(remote, PeerChannel { tx });
        debug!("registered DTLS peer channel for {}", remote);
    }

    /// Main receive loop — runs forever, dispatches packets
    pub async fn run(&self) {
        let mut buf = vec![0u8; config::UDP_RECV_BUF_SIZE];

        loop {
            let (len, remote) = match self.socket.recv_from(&mut buf).await {
                Ok(r) => r,
                Err(e) => {
                    error!("UDP recv error: {e}");
                    continue;
                }
            };

            let packet = &buf[..len];
            trace!("UDP recv {} bytes from {}", len, remote);

            match demux::classify(packet) {
                PacketType::Stun => {
                    self.handle_stun(packet, remote).await;
                }
                PacketType::Dtls => {
                    self.handle_dtls(packet, remote).await;
                }
                PacketType::Srtp => {
                    // TODO: Phase 3
                    trace!("SRTP packet from {} ({} bytes)", remote, len);
                }
                PacketType::Unknown => {
                    warn!(
                        "unknown packet type from {} (first byte: 0x{:02X})",
                        remote, packet[0]
                    );
                }
            }
        }
    }

    async fn handle_stun(&self, buf: &[u8], remote: SocketAddr) {
        match ice::handle_stun_packet(buf, remote, &self.ice_creds) {
            IceResult::SendResponse {
                data,
                remote,
                use_candidate,
            } => {
                if use_candidate {
                    debug!("ICE: USE-CANDIDATE from {} — candidate pair selected", remote);
                    // TODO: trigger DTLS handshake for this peer
                }

                if let Err(e) = self.socket.send_to(&data, remote).await {
                    error!("failed to send STUN response to {}: {e}", remote);
                }
            }
            IceResult::Ignore => {}
        }
    }

    async fn handle_dtls(&self, buf: &[u8], remote: SocketAddr) {
        let peers = self.peers.lock().await;
        if let Some(peer) = peers.get(&remote) {
            if peer.tx.send(buf.to_vec()).await.is_err() {
                warn!("DTLS channel closed for {}", remote);
            }
        } else {
            debug!(
                "DTLS packet from unknown peer {} ({} bytes) — no channel registered",
                remote,
                buf.len()
            );
        }
    }
}
