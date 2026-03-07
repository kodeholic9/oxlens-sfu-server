// author: kodeholic (powered by Claude)
//! Room + RoomHub — 2PC 구조 conference room with O(1) media-path lookups
//!
//! 2PC 구조에서 같은 유저가 sockaddr 2개를 사용한다.
//! (브라우저가 PC마다 별도 UDP 소켓을 바인딩하므로 로컬 포트가 다르다)
//!
//! Lookup paths:
//!   [signaling]  room_id + user_id → Participant                        O(1)
//!   [STUN]       ufrag → room_id → (Participant, PcType)               O(1) × 2
//!   [SRTP]       addr  → room_id → (Participant, PcType)               O(1) × 2
//!
//! Room holds 3 indices:
//!   participants : user_id → Arc<Participant>               (primary, signaling)
//!   by_ufrag     : ufrag   → (Arc<Participant>, PcType)    (STUN cold path)
//!   by_addr      : addr    → (Arc<Participant>, PcType)    (SRTP hot path)
//!
//! RoomHub holds reverse indices:
//!   ufrag_index  : ufrag → room_id
//!   addr_index   : addr  → room_id

use dashmap::DashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tracing::{debug, trace};

use crate::config;
use crate::config::RoomMode;
use crate::error::{LightError, LightResult};
use crate::room::participant::{Participant, PcType};
use crate::room::floor::FloorController;

// ============================================================================
// Room
// ============================================================================

pub struct Room {
    pub id:       String,
    pub name:     String,
    pub capacity: usize,
    pub mode:     RoomMode,
    pub created_at: u64,

    /// Floor Control (PTT 모드에서만 활성)
    pub floor: FloorController,

    /// Primary index: user_id → Participant
    pub participants: DashMap<String, Arc<Participant>>,
    /// STUN index: ufrag → (Participant, PcType)
    /// publish/subscribe 각각의 ufrag가 등록된다
    by_ufrag: DashMap<String, (Arc<Participant>, PcType)>,
    /// Media index: addr → (Participant, PcType)
    /// STUN latch 시 등록, publish/subscribe 각각 별도 addr
    by_addr: DashMap<SocketAddr, (Arc<Participant>, PcType)>,
}

impl Room {
    pub fn new(id: String, name: String, capacity: Option<usize>, mode: RoomMode, created_at: u64) -> Self {
        Self {
            id,
            name,
            capacity: capacity.unwrap_or(config::ROOM_DEFAULT_CAPACITY)
                .min(config::ROOM_MAX_CAPACITY),
            mode,
            created_at,
            floor: FloorController::new(),
            participants: DashMap::new(),
            by_ufrag:     DashMap::new(),
            by_addr:      DashMap::new(),
        }
    }

    /// Add participant — registers in user_id + both ufrag indices (pub/sub)
    pub fn add_participant(&self, p: Arc<Participant>) -> LightResult<()> {
        if self.participants.len() >= self.capacity {
            return Err(LightError::RoomFull);
        }
        if self.participants.contains_key(&p.user_id) {
            return Err(LightError::AlreadyInRoom);
        }

        // ufrag 인덱스: publish + subscribe 각각 등록
        self.by_ufrag.insert(
            p.publish.ufrag.clone(),
            (Arc::clone(&p), PcType::Publish),
        );
        self.by_ufrag.insert(
            p.subscribe.ufrag.clone(),
            (Arc::clone(&p), PcType::Subscribe),
        );

        self.participants.insert(p.user_id.clone(), p);
        Ok(())
    }

    /// Remove participant — cleans all indices (user_id + 2 ufrags + up to 2 addrs)
    pub fn remove_participant(&self, user_id: &str) -> LightResult<Arc<Participant>> {
        let (_, p) = self.participants
            .remove(user_id)
            .ok_or(LightError::NotInRoom)?;

        // ufrag 인덱스 정리
        self.by_ufrag.remove(&p.publish.ufrag);
        self.by_ufrag.remove(&p.subscribe.ufrag);

        // addr 인덱스 정리
        if let Some(addr) = p.publish.get_address() {
            self.by_addr.remove(&addr);
        }
        if let Some(addr) = p.subscribe.get_address() {
            self.by_addr.remove(&addr);
        }

        debug!("participant removed user={} room={}", user_id, self.id);
        Ok(p)
    }

    /// STUN latch: ufrag로 참가자+PcType 찾아서 해당 세션의 addr 등록
    /// Returns (Participant, PcType) or None
    pub fn latch(&self, ufrag: &str, addr: SocketAddr) -> Option<(Arc<Participant>, PcType)> {
        let (p, pc_type) = {
            let entry = self.by_ufrag.get(ufrag)?;
            entry.value().clone()
        };

        let session = p.session(pc_type);

        // 기존 addr이 있으면 제거 (NAT rebinding)
        if let Some(old_addr) = session.get_address() {
            if old_addr != addr {
                self.by_addr.remove(&old_addr);
                trace!("NAT rebind user={} pc={} {}→{}", p.user_id, pc_type, old_addr, addr);
            }
        }

        session.latch_address(addr);
        self.by_addr.insert(addr, (Arc::clone(&p), pc_type));
        trace!("latch user={} pc={} ufrag={} addr={}", p.user_id, pc_type, ufrag, addr);
        Some((p, pc_type))
    }

    // --- O(1) lookups ---

    pub fn get_participant(&self, user_id: &str) -> Option<Arc<Participant>> {
        self.participants.get(user_id).map(|e| e.value().clone())
    }

    pub fn get_by_ufrag(&self, ufrag: &str) -> Option<(Arc<Participant>, PcType)> {
        self.by_ufrag.get(ufrag).map(|e| e.value().clone())
    }

    pub fn get_by_addr(&self, addr: &SocketAddr) -> Option<(Arc<Participant>, PcType)> {
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

    /// SSRC로 publisher 찾기 (zero-alloc, DashMap iter 직접 순회)
    /// NACK/RTCP relay에서 all_participants().find() 대체
    pub fn find_by_track_ssrc(&self, ssrc: u32) -> Option<Arc<Participant>> {
        self.participants.iter().find_map(|entry| {
            let p = entry.value();
            let tracks = p.tracks.lock().unwrap();
            if tracks.iter().any(|t| t.ssrc == ssrc) {
                Some(Arc::clone(p))
            } else {
                None
            }
        })
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

    pub fn create(&self, name: String, capacity: Option<usize>, mode: RoomMode, created_at: u64) -> Arc<Room> {
        let id = uuid::Uuid::new_v4().to_string();
        let room = Arc::new(Room::new(id.clone(), name, capacity, mode, created_at));
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

        // Clean reverse indices for all participants (both sessions)
        for entry in room.participants.iter() {
            let p = entry.value();
            // ufrag 역인덱스 정리 (publish + subscribe)
            self.ufrag_index.remove(&p.publish.ufrag);
            self.ufrag_index.remove(&p.subscribe.ufrag);
            // addr 역인덱스 정리
            if let Some(addr) = p.publish.get_address() {
                self.addr_index.remove(&addr);
            }
            if let Some(addr) = p.subscribe.get_address() {
                self.addr_index.remove(&addr);
            }
        }

        Ok(room)
    }

    pub fn count(&self) -> usize {
        self.rooms.len()
    }

    // --- participant lifecycle ---

    /// Register participant in room + ufrag reverse indices (both pub/sub)
    pub fn add_participant(&self, room_id: &str, p: Arc<Participant>) -> LightResult<()> {
        let room = self.get(room_id)?;
        let pub_ufrag = p.publish.ufrag.clone();
        let sub_ufrag = p.subscribe.ufrag.clone();
        room.add_participant(p)?;
        self.ufrag_index.insert(pub_ufrag, room_id.to_string());
        self.ufrag_index.insert(sub_ufrag, room_id.to_string());
        Ok(())
    }

    /// Remove participant from room + clean all reverse indices
    pub fn remove_participant(&self, room_id: &str, user_id: &str) -> LightResult<Arc<Participant>> {
        let room = self.get(room_id)?;
        let p = room.remove_participant(user_id)?;
        // ufrag 역인덱스 정리
        self.ufrag_index.remove(&p.publish.ufrag);
        self.ufrag_index.remove(&p.subscribe.ufrag);
        // addr 역인덱스 정리
        if let Some(addr) = p.publish.get_address() {
            self.addr_index.remove(&addr);
        }
        if let Some(addr) = p.subscribe.get_address() {
            self.addr_index.remove(&addr);
        }
        Ok(p)
    }

    /// STUN latch: ufrag → find room → latch addr → register addr reverse index
    /// Returns (Participant, PcType, Room)
    pub fn latch_by_ufrag(
        &self,
        ufrag: &str,
        addr: SocketAddr,
    ) -> Option<(Arc<Participant>, PcType, Arc<Room>)> {
        let room_id = self.ufrag_index.get(ufrag)?.value().clone();
        let room = self.rooms.get(&room_id)?.value().clone();
        let (p, pc_type) = room.latch(ufrag, addr)?;
        self.addr_index.insert(addr, room_id);
        Some((p, pc_type, room))
    }

    /// SRTP hot path: addr → room → (participant, pc_type) + room
    /// O(1) × 2
    pub fn find_by_addr(&self, addr: &SocketAddr) -> Option<(Arc<Participant>, PcType, Arc<Room>)> {
        let room_id = self.addr_index.get(addr)?.value().clone();
        let room = self.rooms.get(&room_id)?.value().clone();
        let (p, pc_type) = room.get_by_addr(addr)?;
        Some((p, pc_type, room))
    }

    /// STUN lookup (without latch): ufrag → (participant, pc_type, room)
    pub fn find_by_ufrag(&self, ufrag: &str) -> Option<(Arc<Participant>, PcType, Arc<Room>)> {
        let room_id = self.ufrag_index.get(ufrag)?.value().clone();
        let room = self.rooms.get(&room_id)?.value().clone();
        let (p, pc_type) = room.get_by_ufrag(ufrag)?;
        Some((p, pc_type, room))
    }

    /// 좀비 세션 정리: last_seen + timeout < now 인 참가자 제거
    /// 반환: (room_id, participant) 목록 (broadcast 용도)
    pub fn reap_zombies(&self, now_ms: u64, timeout_ms: u64) -> Vec<(String, Arc<Participant>)> {
        let mut reaped = Vec::new();

        // 모든 room 순회
        let room_ids: Vec<String> = self.rooms.iter()
            .map(|e| e.key().clone())
            .collect();

        for room_id in &room_ids {
            let room = match self.rooms.get(room_id) {
                Some(r) => r.value().clone(),
                None => continue,
            };

            // 좀비 판별: last_seen 기준
            let zombie_ids: Vec<String> = room.participants.iter()
                .filter(|entry| {
                    let p = entry.value();
                    let last = p.last_seen.load(std::sync::atomic::Ordering::Relaxed);
                    last > 0 && now_ms.saturating_sub(last) > timeout_ms
                })
                .map(|entry| entry.key().clone())
                .collect();

            for user_id in zombie_ids {
                match self.remove_participant(room_id, &user_id) {
                    Ok(p) => {
                        reaped.push((room_id.clone(), p));
                    }
                    Err(_) => {} // 이미 제거됨 (race)
                }
            }
        }

        reaped
    }
}
