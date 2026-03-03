// author: kodeholic (powered by Claude)
//! Opcode definitions for signaling protocol
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
pub const SDP_OFFER: u16 = 15;
pub const ICE_CANDIDATE: u16 = 16;
pub const MESSAGE: u16 = 20;

// --- Server → Client (Event) ---
pub const HELLO: u16 = 0;
pub const ROOM_EVENT: u16 = 100;
pub const TRACK_EVENT: u16 = 101;
pub const SERVER_ICE_CANDIDATE: u16 = 102;
pub const MESSAGE_EVENT: u16 = 103;
