// author: kodeholic (powered by Claude)
//! DemuxConn — virtual connection adapter bridging demux UDP loop with DTLSConn.
//!
//! Architecture:
//!   Real UDP socket (we own)
//!       │
//!       ├── STUN → ICE handler (direct)
//!       ├── DTLS → mpsc channel → DemuxConn → DTLSConn
//!       └── SRTP → media pipeline (later)

use async_trait::async_trait;
use std::any::Any;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, Mutex};
use webrtc_util::conn::Conn;

/// Inbound DTLS packet sender (held by UDP recv loop)
pub type DtlsPacketTx = mpsc::Sender<Vec<u8>>;

/// Virtual connection for a single participant's DTLS session.
pub struct DemuxConn {
    socket:    Arc<UdpSocket>,
    peer_addr: SocketAddr,
    rx:        Mutex<mpsc::Receiver<Vec<u8>>>,
}

impl DemuxConn {
    /// Returns (adapter, tx). Feed DTLS packets into tx.
    pub fn new(socket: Arc<UdpSocket>, peer_addr: SocketAddr) -> (Self, DtlsPacketTx) {
        let (tx, rx) = mpsc::channel(128);
        let conn = Self {
            socket,
            peer_addr,
            rx: Mutex::new(rx),
        };
        (conn, tx)
    }
}

#[async_trait]
impl Conn for DemuxConn {
    async fn connect(&self, _addr: SocketAddr) -> webrtc_util::Result<()> {
        Ok(())
    }

    async fn recv(&self, buf: &mut [u8]) -> webrtc_util::Result<usize> {
        let mut rx = self.rx.lock().await;
        match rx.recv().await {
            Some(data) => {
                let len = data.len().min(buf.len());
                buf[..len].copy_from_slice(&data[..len]);
                Ok(len)
            }
            None => Err(webrtc_util::Error::Other("dtls rx channel closed".into())),
        }
    }

    async fn recv_from(&self, buf: &mut [u8]) -> webrtc_util::Result<(usize, SocketAddr)> {
        let n = self.recv(buf).await?;
        Ok((n, self.peer_addr))
    }

    async fn send(&self, buf: &[u8]) -> webrtc_util::Result<usize> {
        self.socket
            .send_to(buf, self.peer_addr)
            .await
            .map_err(|e| webrtc_util::Error::Other(e.to_string()))
    }

    async fn send_to(&self, buf: &[u8], _target: SocketAddr) -> webrtc_util::Result<usize> {
        self.send(buf).await
    }

    fn local_addr(&self) -> webrtc_util::Result<SocketAddr> {
        self.socket
            .local_addr()
            .map_err(|e| webrtc_util::Error::Other(e.to_string()))
    }

    fn remote_addr(&self) -> Option<SocketAddr> {
        Some(self.peer_addr)
    }

    async fn close(&self) -> webrtc_util::Result<()> {
        Ok(())
    }

    fn as_any(&self) -> &(dyn Any + Send + Sync) {
        self
    }
}
