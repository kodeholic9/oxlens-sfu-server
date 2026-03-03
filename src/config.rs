// author: kodeholic (powered by Claude)
//! Global configuration constants

// --- Server ---
pub const WS_PORT: u16 = 8080;
pub const UDP_PORT: u16 = 10000;

// --- Heartbeat ---
pub const HEARTBEAT_INTERVAL_MS: u64 = 30_000;
pub const HEARTBEAT_TIMEOUT_MS: u64 = 90_000;

// --- Room ---
pub const ROOM_MAX_CAPACITY: usize = 20;
pub const ROOM_DEFAULT_CAPACITY: usize = 10;

// --- Signaling ---
/// ACK timeout: pid에 대한 응답 대기 시간
pub const ACK_TIMEOUT_MS: u64 = 5_000;
/// ACK 미수신 누적 한도 (초과 시 연결 종료)
pub const ACK_MISS_LIMIT: u32 = 3;

// --- Transport ---
/// 패킷 demux: 첫 바이트 기반 분류 (RFC 5764)
pub const DEMUX_STUN_MIN: u8 = 0x00;
pub const DEMUX_STUN_MAX: u8 = 0x03;
pub const DEMUX_DTLS_MIN: u8 = 0x14;
pub const DEMUX_DTLS_MAX: u8 = 0x3F;
pub const DEMUX_RTP_MIN: u8 = 0x80;
pub const DEMUX_RTP_MAX: u8 = 0xBF;

// --- Media ---
pub const RTP_HEADER_MIN_SIZE: usize = 12;
pub const UDP_RECV_BUF_SIZE: usize = 2048;
