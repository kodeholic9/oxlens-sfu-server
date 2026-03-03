// author: kodeholic (powered by Claude)
//! SSRC-based media routing table
//!
//! Hot path: recv packet → lookup SSRC → find room → fan-out to subscribers
//! Uses DashMap for segment-level locking (better than RwLock<HashMap> at 20 participants)

use dashmap::DashMap;
use std::sync::Arc;

use crate::media::track::TrackContext;

pub struct Router {
    /// SSRC → track context (O(1) lookup on hot path)
    pub by_ssrc: DashMap<u32, Arc<TrackContext>>,
}

impl Router {
    pub fn new() -> Self {
        Self {
            by_ssrc: DashMap::new(),
        }
    }

    pub fn register(&self, ssrc: u32, ctx: Arc<TrackContext>) {
        self.by_ssrc.insert(ssrc, ctx);
    }

    pub fn unregister(&self, ssrc: u32) {
        self.by_ssrc.remove(&ssrc);
    }

    pub fn lookup(&self, ssrc: u32) -> Option<Arc<TrackContext>> {
        self.by_ssrc.get(&ssrc).map(|entry| entry.value().clone())
    }
}
