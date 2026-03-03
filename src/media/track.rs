// author: kodeholic (powered by Claude)
//! Track context for media routing

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::Mutex;

#[derive(Debug)]
pub struct TrackContext {
    pub ssrc: u32,
    pub participant_id: String,
    pub room_id: String,
    pub kind: TrackKind,
    /// Remote address (updated via ICE consent / STUN binding)
    pub remote_addr: Mutex<Option<SocketAddr>>,
    pub last_seen: AtomicU64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackKind {
    Audio,
    Video,
}

impl TrackContext {
    pub fn new(ssrc: u32, participant_id: String, room_id: String, kind: TrackKind) -> Self {
        Self {
            ssrc,
            participant_id,
            room_id,
            kind,
            remote_addr: Mutex::new(None),
            last_seen: AtomicU64::new(0),
        }
    }

    pub fn touch(&self, ts: u64) {
        self.last_seen.store(ts, Ordering::Relaxed);
    }
}
