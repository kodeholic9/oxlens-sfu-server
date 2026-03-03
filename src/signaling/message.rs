// author: kodeholic (powered by Claude)
//! Signaling message types
//!
//! Packet format:
//!   { "op": N, "pid": u64, "d": { ... } }              — request / event
//!   { "op": N, "pid": u64, "ok": true,  "d": { ... } } — success response
//!   { "op": N, "pid": u64, "ok": false, "d": { "code": u16, "msg": "..." } } — error response
//!
//! Rules:
//!   - `pid` is ALWAYS present (sequential, per-side counter starting from 1)
//!   - `ok` is present ONLY in responses
//!   - No `ok` field → new request or event (initiator)
//!   - `ok` field present → response to a previously received pid

use serde::{Deserialize, Serialize};

/// Raw packet from/to WebSocket
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Packet {
    pub op: u16,
    pub pid: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ok: Option<bool>,
    #[serde(default)]
    pub d: serde_json::Value,
}

impl Packet {
    /// Create a new request/event packet (no `ok` field)
    pub fn new(op: u16, pid: u64, d: serde_json::Value) -> Self {
        Self { op, pid, ok: None, d }
    }

    /// Create a success response
    pub fn ok(op: u16, pid: u64, d: serde_json::Value) -> Self {
        Self { op, pid, ok: Some(true), d }
    }

    /// Create an error response
    pub fn err(op: u16, pid: u64, code: u16, msg: &str) -> Self {
        Self {
            op,
            pid,
            ok: Some(false),
            d: serde_json::json!({ "code": code, "msg": msg }),
        }
    }

    /// Is this a response (has `ok` field)?
    pub fn is_response(&self) -> bool {
        self.ok.is_some()
    }
}

// --- Request payloads (Client → Server) ---

#[derive(Debug, Deserialize)]
pub struct IdentifyRequest {
    pub token: String,
}

#[derive(Debug, Deserialize)]
pub struct RoomCreateRequest {
    pub name: String,
    #[serde(default)]
    pub capacity: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct RoomJoinRequest {
    pub room_id: String,
    #[serde(default)]
    pub sdp_offer: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RoomLeaveRequest {
    pub room_id: String,
}

#[derive(Debug, Deserialize)]
pub struct SdpOfferRequest {
    pub sdp: String,
}

#[derive(Debug, Deserialize)]
pub struct IceCandidatePayload {
    pub candidate: String,
    #[serde(default)]
    pub sdp_mid: Option<String>,
    #[serde(default)]
    pub sdp_mline_index: Option<u16>,
}

#[derive(Debug, Deserialize)]
pub struct MessageRequest {
    pub room_id: String,
    pub content: String,
}

// --- Event payloads (Server → Client) ---

#[derive(Debug, Serialize)]
pub struct HelloEvent {
    pub heartbeat_interval: u64,
}

#[derive(Debug, Serialize)]
pub struct RoomEventPayload {
    #[serde(rename = "type")]
    pub event_type: String, // "participant_joined", "participant_left", "room_deleted"
    pub room_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct TrackEventPayload {
    #[serde(rename = "type")]
    pub event_type: String, // "track_added", "track_removed"
    pub participant_id: String,
    #[serde(default)]
    pub tracks: Vec<TrackInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackInfo {
    pub track_id: String,
    pub kind: String, // "audio", "video"
    pub ssrc: u32,
}
