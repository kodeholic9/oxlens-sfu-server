// author: kodeholic (powered by Claude)
//! Room + RoomHub — conference room state with O(1) media-path lookups
//!
//! Lookup paths:
//!   [signaling]  room_id + user_id → Participant        O(1)
//!   [STUN]       ufrag → room_id → Participant          O(1) × 2
//!   [SRTP]       addr  → room_id → Participant          O(1) × 2
//!
//! Room holds 3 indices into the same Arc<Participant>:
//!   participants : user_id → Arc<Participant>   (primary, signaling)
//!   by_ufrag     : ufrag   → Arc<Participant>   (STUN cold path)
//!   by_addr      : addr    → Arc<Participant>   (SRTP hot path)
//!
//! RoomHub holds reverse indices:
//!   ufrag_index  : ufrag → room_id
//!   addr_index   : addr  → room_id

use dashmap::DashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tracing::{debug, trace};

use crate::config;
use crate::error::{LightError, LightResult};
use crate::room::participant::Participant;

// ============================================================================
// Room
// ============================================================================

pub struct Room {
    pub id:       String,
    pub name:     String,
    pub capacity: usize,
    pub created_at: u64,

    /// Primary index: user_id → Participant
    pub participants: DashMap<String, Arc<Participant>>,
    /// STUN index: ufrag → Participant (populated on join)
    by_ufrag: DashMap<String, Arc<Participant>>,
    /// Media index: addr → Participant (populated on STUN latch)
    by_addr: DashMap<SocketAddr, Arc<Participant>>,
}

impl Room {
    pub fn new(id: String, name: String, capacity: Option<usize>, created_at: u64) -> Self {
        Self {
            id,
            name,
            capacity: capacity.unwrap_or(config::ROOM_DEFAULT_CAPACITY)
                .min(config::ROOM_MAX_CAPACITY),
            created_at,
            participants: DashMap::new(),
            by_ufrag:     DashMap::new(),
            by_addr:      DashMap::new(),
        }
    }

    /// Add participant — registers in both user_id and ufrag indices
    pub fn add_participant(&self, p: Arc<Participant>) -> LightResult<()> {
        if self.participants.len() >= self.capacity {
            return Err(LightError::RoomFull);
        }
        if self.participants.contains_key(&p.user_id) {
            return Err(LightError::AlreadyInRoom);
        }
        self.by_ufrag.insert(p.ufrag.clone(), Arc::clone(&p));
        self.participants.insert(p.user_id.clone(), p);
        Ok(())
    }

    /// Remove participant — cleans all 3 indices
    pub fn remove_participant(&self, user_id: &str) -> LightResult<Arc<Participant>> {
        let (_, p) = self.participants
            .remove(user_id)
            .ok_or(LightError::NotInRoom)?;

        self.by_ufrag.remove(&p.ufrag);
        if let Some(addr) = p.get_address() {
            self.by_addr.remove(&addr);
        }
        debug!("participant removed user={} room={}", user_id, self.id);
        Ok(p)
    }

    /// STUN latch: register addr index + update participant address
    /// Returns true if latch succeeded (participant found)
    pub fn latch(&self, ufrag: &str, addr: SocketAddr) -> Option<Arc<Participant>> {
        let p = self.by_ufrag.get(ufrag)?.value().clone();

        // 기존 addr이 있으면 제거 (NAT rebinding)
        if let Some(old_addr) = p.get_address() {
            if old_addr != addr {
                self.by_addr.remove(&old_addr);
                trace!("NAT rebind user={} {}→{}", p.user_id, old_addr, addr);
            }
        }

        p.latch_address(addr);
        self.by_addr.insert(addr, Arc::clone(&p));
        trace!("latch user={} ufrag={} addr={}", p.user_id, ufrag, addr);
        Some(p)
    }

    // --- O(1) lookups ---

    pub fn get_participant(&self, user_id: &str) -> Option<Arc<Participant>> {
        self.participants.get(user_id).map(|e| e.value().clone())
    }

    pub fn get_by_ufrag(&self, ufrag: &str) -> Option<Arc<Participant>> {
        self.by_ufrag.get(ufrag).map(|e| e.value().clone())
    }

    pub fn get_by_addr(&self, addr: &SocketAddr) -> Option<Arc<Participant>> {
        self.by_addr.get(addr).map(|e| e.value().clone())
    }

    // --- collection queries ---

    pub fn participant_count(&self) -> usize {
        self.participants.len()
    }

    pub fn member_ids(&self) -> Vec<String> {
        self.participants.iter().map(|e| e.key().clone()).collect()
    }

    pub fn all_participants(&self) -> Vec<Arc<Participant>> {
        self.participants.iter().map(|e| e.value().clone()).collect()
    }

    /// All participants except one (for relay/broadcast)
    pub fn other_participants(&self, exclude_user: &str) -> Vec<Arc<Participant>> {
        self.participants
            .iter()
            .filter(|e| e.key() != exclude_user)
            .map(|e| e.value().clone())
            .collect()
    }
}

// ============================================================================
// RoomHub
// ============================================================================

pub struct RoomHub {
    /// Primary: room_id → Room
    pub rooms: DashMap<String, Arc<Room>>,
    /// Reverse index: ufrag → room_id (STUN cold path)
    ufrag_index: DashMap<String, String>,
    /// Reverse index: addr → room_id (SRTP hot path)
    addr_index: DashMap<SocketAddr, String>,
}

impl RoomHub {
    pub fn new() -> Self {
        Self {
            rooms:       DashMap::new(),
            ufrag_index: DashMap::new(),
            addr_index:  DashMap::new(),
        }
    }

    pub fn create(&self, name: String, capacity: Option<usize>, created_at: u64) -> Arc<Room> {
        let id = uuid::Uuid::new_v4().to_string();
        let room = Arc::new(Room::new(id.clone(), name, capacity, created_at));
        self.rooms.insert(id, room.clone());
        room
    }

    pub fn get(&self, room_id: &str) -> LightResult<Arc<Room>> {
        self.rooms
            .get(room_id)
            .map(|e| e.value().clone())
            .ok_or(LightError::RoomNotFound)
    }

    pub fn remove_room(&self, room_id: &str) -> LightResult<Arc<Room>> {
        let (_, room) = self.rooms
            .remove(room_id)
            .ok_or(LightError::RoomNotFound)?;

        // Clean reverse indices for all participants
        for entry in room.participants.iter() {
            let p = entry.value();
            self.ufrag_index.remove(&p.ufrag);
            if let Some(addr) = p.get_address() {
                self.addr_index.remove(&addr);
            }
        }

        Ok(room)
    }

    pub fn count(&self) -> usize {
        self.rooms.len()
    }

    // --- participant lifecycle (delegates to Room + maintains reverse index) ---

    /// Register participant in room + ufrag reverse index
    pub fn add_participant(&self, room_id: &str, p: Arc<Participant>) -> LightResult<()> {
        let room = self.get(room_id)?;
        let ufrag = p.ufrag.clone();
        room.add_participant(p)?;
        self.ufrag_index.insert(ufrag, room_id.to_string());
        Ok(())
    }

    /// Remove participant from room + clean reverse indices
    pub fn remove_participant(&self, room_id: &str, user_id: &str) -> LightResult<Arc<Participant>> {
        let room = self.get(room_id)?;
        let p = room.remove_participant(user_id)?;
        self.ufrag_index.remove(&p.ufrag);
        if let Some(addr) = p.get_address() {
            self.addr_index.remove(&addr);
        }
        Ok(p)
    }

    /// STUN latch: ufrag → find room → latch addr → register addr reverse index
    /// Returns (Participant, Room) or None
    pub fn latch_by_ufrag(
        &self,
        ufrag: &str,
        addr: SocketAddr,
    ) -> Option<(Arc<Participant>, Arc<Room>)> {
        let room_id = self.ufrag_index.get(ufrag)?.value().clone();
        let room = self.rooms.get(&room_id)?.value().clone();
        let p = room.latch(ufrag, addr)?;
        self.addr_index.insert(addr, room_id);
        Some((p, room))
    }

    /// SRTP hot path: addr → room → participant + room
    /// O(1) × 2 — no iteration
    pub fn find_by_addr(&self, addr: &SocketAddr) -> Option<(Arc<Participant>, Arc<Room>)> {
        let room_id = self.addr_index.get(addr)?.value().clone();
        let room = self.rooms.get(&room_id)?.value().clone();
        let p = room.get_by_addr(addr)?;
        Some((p, room))
    }

    /// STUN lookup (without latch): ufrag → participant + room
    pub fn find_by_ufrag(&self, ufrag: &str) -> Option<(Arc<Participant>, Arc<Room>)> {
        let room_id = self.ufrag_index.get(ufrag)?.value().clone();
        let room = self.rooms.get(&room_id)?.value().clone();
        let p = room.get_by_ufrag(ufrag)?;
        Some((p, room))
    }
}
