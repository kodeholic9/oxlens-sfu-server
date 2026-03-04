// author: kodeholic (powered by Claude)
//! Opcode definitions for signaling protocol (2PC / SDP-free)
//!
//! Request/Response: Client sends request, Server responds with same op + ok field.
//! Event:           Server sends event, Client responds with same op + ok field.
//! All messages carry a sequential `pid` for request-response pairing.

// --- Client → Server (Request) ---
pub const HEARTBEAT: u16 = 1;
pub const IDENTIFY: u16 = 3;
pub const ROOM_LIST: u16 = 9;
pub const ROOM_CREATE: u16 = 10;
pub const ROOM_JOIN: u16 = 11;
pub const ROOM_LEAVE: u16 = 12;
pub const PUBLISH_TRACKS: u16 = 15;   // 클라이언트가 자기 트랙 SSRC 등록
pub const MUTE_UPDATE: u16 = 17;      // 트랙 mute/unmute 상태 변경
pub const MESSAGE: u16 = 20;

// --- Server → Client (Event) ---
pub const HELLO: u16 = 0;
pub const ROOM_EVENT: u16 = 100;
pub const TRACKS_UPDATE: u16 = 101;   // 트랙 추가/제거 통보
pub const TRACK_STATE: u16 = 102;     // 트랙 mute/unmute 상태 브로드캐스트
pub const MESSAGE_EVENT: u16 = 103;
