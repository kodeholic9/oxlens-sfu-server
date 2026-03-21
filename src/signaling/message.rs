// author: kodeholic (powered by Claude)
//! Signaling message types (2PC / SDP-free)
//!
//! Packet format:
//!   { "op": N, "pid": u64, "d": { ... } }              — request / event
//!   { "op": N, "pid": u64, "ok": true,  "d": { ... } } — success response
//!   { "op": N, "pid": u64, "ok": false, "d": { "code": u16, "msg": "..." } } — error response

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
    pub fn new(op: u16, pid: u64, d: serde_json::Value) -> Self {
        Self { op, pid, ok: None, d }
    }

    pub fn ok(op: u16, pid: u64, d: serde_json::Value) -> Self {
        Self { op, pid, ok: Some(true), d }
    }

    pub fn err(op: u16, pid: u64, code: u16, msg: &str) -> Self {
        Self {
            op,
            pid,
            ok: Some(false),
            d: serde_json::json!({ "code": code, "msg": msg }),
        }
    }

    pub fn is_response(&self) -> bool {
        self.ok.is_some()
    }
}

// --- Request payloads (Client → Server) ---

#[derive(Debug, Deserialize)]
pub struct IdentifyRequest {
    pub token: String,
    #[serde(default)]
    pub user_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RoomCreateRequest {
    pub name: String,
    #[serde(default)]
    pub capacity: Option<usize>,
    #[serde(default)]
    pub mode: Option<RoomModeField>,
    /// Simulcast 활성화 여부 (Conference 모드에서만 의미)
    #[serde(default)]
    pub simulcast: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct RoomJoinRequest {
    pub room_id: String,
    // SDP-free: sdp_offer 제거됨. 서버가 server_config으로 응답.
}

#[derive(Debug, Deserialize)]
pub struct RoomLeaveRequest {
    pub room_id: String,
}

/// 클라이언트가 자기 트랙 SSRC 등록
#[derive(Debug, Deserialize)]
pub struct PublishTracksRequest {
    pub tracks: Vec<PublishTrackItem>,
    /// Chrome offerer가 할당한 TWCC extmap ID (simulcast 모드에서 클라이언트가 전달)
    #[serde(default)]
    pub twcc_extmap_id: Option<u8>,
}

#[derive(Debug, Deserialize)]
pub struct PublishTrackItem {
    pub kind: String,  // "audio" | "video"
    pub ssrc: u32,
    /// Simulcast RTP stream ID ("h" | "l", None for non-simulcast)
    #[serde(default)]
    pub rid: Option<String>,
}

/// 트랙 mute/unmute 상태 변경 요청
#[derive(Debug, Deserialize)]
pub struct MuteUpdateRequest {
    pub ssrc: u32,
    pub muted: bool,
}

#[derive(Debug, Deserialize)]
pub struct MessageRequest {
    pub room_id: String,
    pub content: String,
}

/// 카메라 웜업 완료 알림 (PLI 트리거 + VIDEO_RESUMED 브로드캐스트)
#[derive(Debug, Deserialize)]
pub struct CameraReadyRequest {
    pub room_id: String,
}

/// 클라이언트가 인식한 subscribe SSRC 목록 (TRACKS_ACK)
#[derive(Debug, Deserialize)]
pub struct TracksAckRequest {
    /// 클라이언트가 현재 인식하고 있는 active primary SSRC 목록 (RTX 제외)
    pub ssrcs: Vec<u32>,
}

// --- Floor Control (MCPTT/MBCP) ---

#[derive(Debug, Deserialize)]
pub struct FloorRequestMsg {
    pub room_id: String,
    /// 우선순위 (0~255, 기본값 0). 3GPP TS 24.380 Floor Priority.
    #[serde(default)]
    pub priority: Option<u8>,
}

#[derive(Debug, Deserialize)]
pub struct FloorReleaseMsg {
    pub room_id: String,
}

#[derive(Debug, Deserialize)]
pub struct FloorPingMsg {
    pub room_id: String,
}

#[derive(Debug, Deserialize)]
pub struct FloorQueuePosMsg {
    pub room_id: String,
}

/// ROOM_CREATE 요청의 mode 필드 용
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RoomModeField {
    Conference,
    Ptt,
}

impl Default for RoomModeField {
    fn default() -> Self { RoomModeField::Conference }
}

impl RoomModeField {
    pub fn to_config(&self) -> crate::config::RoomMode {
        match self {
            RoomModeField::Conference => crate::config::RoomMode::Conference,
            RoomModeField::Ptt => crate::config::RoomMode::Ptt,
        }
    }
}

// --- Event payloads (Server → Client) ---

#[derive(Debug, Serialize)]
pub struct HelloEvent {
    pub heartbeat_interval: u64,
}

#[derive(Debug, Serialize)]
pub struct RoomEventPayload {
    #[serde(rename = "type")]
    pub event_type: String,
    pub room_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackInfo {
    pub user_id: String,
    pub track_id: String,
    pub kind: String,
    pub ssrc: u32,
}

/// Simulcast 레이어 선택 요청 (Phase 3)
#[derive(Debug, Deserialize)]
pub struct SubscribeLayerRequest {
    pub targets: Vec<SubscribeLayerTarget>,
}

#[derive(Debug, Deserialize)]
pub struct SubscribeLayerTarget {
    pub user_id: String,  // publisher user_id
    pub rid: String,      // "h", "l", "pause"
}
