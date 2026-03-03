// author: kodeholic (powered by Claude)
//! Participant — per-user state including signaling + media transport
//!
//! Each participant owns:
//!   - WS channel (signaling)
//!   - ICE credentials (ufrag/pwd for STUN verification)
//!   - DTLS session (via DemuxConn)
//!   - SRTP inbound/outbound contexts
//!   - Published tracks (SSRC-based)

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use tokio::sync::mpsc;
use tracing::trace;

use crate::transport::srtp::SrtpContext;

// ============================================================================
// Track
// ============================================================================

#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub enum TrackKind {
    Audio,
    Video,
}

#[derive(Debug, Clone)]
pub struct Track {
    pub ssrc: u32,
    pub kind: TrackKind,
}

// ============================================================================
// Participant
// ============================================================================

pub struct Participant {
    // --- identity ---
    pub user_id:    String,
    pub room_id:    String,
    pub joined_at:  u64,
    pub last_seen:  AtomicU64,

    // --- signaling ---
    /// Send JSON messages to this participant's WebSocket
    pub ws_tx: mpsc::UnboundedSender<String>,

    // --- ICE ---
    pub ufrag:   String,   // server-generated, immutable lookup key
    pub ice_pwd: String,   // STUN MESSAGE-INTEGRITY verification

    // --- transport ---
    /// Latched UDP address (set after first valid STUN Binding Request)
    pub address: Mutex<Option<SocketAddr>>,

    // --- SRTP (per-participant, shared across all BUNDLE tracks) ---
    pub inbound_srtp:  Mutex<SrtpContext>,  // browser → server (decrypt)
    pub outbound_srtp: Mutex<SrtpContext>,  // server → browser (encrypt)

    // --- tracks ---
    /// Published tracks (populated from SDP offer)
    pub tracks: Mutex<Vec<Track>>,
}

impl Participant {
    pub fn new(
        user_id:   String,
        room_id:   String,
        ufrag:     String,
        ice_pwd:   String,
        ws_tx:     mpsc::UnboundedSender<String>,
        joined_at: u64,
    ) -> Self {
        trace!("Participant::new user={} room={} ufrag={}", user_id, room_id, ufrag);
        Self {
            user_id,
            room_id,
            joined_at,
            last_seen:     AtomicU64::new(joined_at),
            ws_tx,
            ufrag,
            ice_pwd,
            address:       Mutex::new(None),
            inbound_srtp:  Mutex::new(SrtpContext::new()),
            outbound_srtp: Mutex::new(SrtpContext::new()),
            tracks:        Mutex::new(Vec::new()),
        }
    }

    pub fn touch(&self, ts: u64) {
        self.last_seen.store(ts, Ordering::Relaxed);
    }

    /// STUN latch: set confirmed UDP address
    pub fn latch_address(&self, addr: SocketAddr) {
        *self.address.lock().unwrap() = Some(addr);
    }

    pub fn get_address(&self) -> Option<SocketAddr> {
        *self.address.lock().unwrap()
    }

    /// Register a track (deduplicated by SSRC)
    pub fn add_track(&self, ssrc: u32, kind: TrackKind) {
        let mut tracks = self.tracks.lock().unwrap();
        if !tracks.iter().any(|t| t.ssrc == ssrc) {
            tracks.push(Track { ssrc, kind });
            trace!("track added ssrc={} user={}", ssrc, self.user_id);
        }
    }

    /// Check if SRTP keys are installed (DTLS handshake completed)
    pub fn is_media_ready(&self) -> bool {
        self.inbound_srtp.lock().unwrap().is_ready()
    }

    /// Install SRTP keys after DTLS handshake
    pub fn install_srtp_keys(
        &self,
        client_key:  &[u8],
        client_salt: &[u8],
        server_key:  &[u8],
        server_salt: &[u8],
    ) {
        self.inbound_srtp.lock().unwrap().install_key(client_key, client_salt);
        self.outbound_srtp.lock().unwrap().install_key(server_key, server_salt);
        trace!("SRTP keys installed user={}", self.user_id);
    }
}
