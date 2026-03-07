// author: kodeholic (powered by Claude)
//! Global configuration constants

// --- Server ---
pub const WS_PORT: u16 = 1974;
pub const UDP_PORT: u16 = 19740;

// --- Heartbeat ---
pub const HEARTBEAT_INTERVAL_MS: u64 = 30_000;
pub const HEARTBEAT_TIMEOUT_MS: u64 = 90_000;

// --- Zombie reaper ---
/// 좀비 세션 검사 주기 (ms)
pub const REAPER_INTERVAL_MS: u64 = 30_000;
/// 미디어/시그널링 무활동 좀비 판정 시간 (ms)
/// HEARTBEAT_TIMEOUT_MS와 동일하게 유지 (WS 끊김 + UDP 무응답 모두 커버)
pub const ZOMBIE_TIMEOUT_MS: u64 = 120_000;

// --- Graceful shutdown ---
/// shutdown 시 drain 대기 시간 (ms)
pub const SHUTDOWN_DRAIN_MS: u64 = 3_000;

// --- Room ---
pub const ROOM_MAX_CAPACITY: usize = 1000;
pub const ROOM_DEFAULT_CAPACITY: usize = 1000;

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

// --- UDP Worker (Phase W-2) ---
/// UDP worker 수 (0 = auto = 코어 수, Linux SO_REUSEPORT)
/// Windows에서는 무시됨 (항상 single worker)
pub const UDP_WORKER_COUNT: usize = 0;

// --- Egress (Phase W-3: subscriber별 egress task) ---
/// subscriber당 egress 큐 크기 (bounded mpsc)
/// 30fps × ~8초분. 큐 풀 시 try_send 실패 = backpressure 드롭 (NACK/RTX가 커버)
pub const EGRESS_QUEUE_SIZE: usize = 256;

// --- Media ---
pub const RTP_HEADER_MIN_SIZE: usize = 12;
pub const UDP_RECV_BUF_SIZE: usize = 2048;

// --- RTX (RFC 4588) ---
/// RTP 캐시 링버퍼 크기 (seq % SIZE로 인덱싱, 약 4초분 @30fps)
pub const RTP_CACHE_SIZE: usize = 512;
/// RTX payload type (server_codec_policy의 rtx_pt와 일치해야 함)
pub const RTX_PAYLOAD_TYPE: u8 = 97;
/// NACK RTCP payload type (RFC 4585, Generic NACK)
pub const RTCP_PT_NACK: u8 = 205;
/// NACK feedback message type (FMT=1)
pub const RTCP_FMT_NACK: u8 = 1;

// --- RTCP Transparent Relay (Phase C-2) ---
/// Sender Report
pub const RTCP_PT_SR: u8 = 200;
/// Receiver Report
pub const RTCP_PT_RR: u8 = 201;
/// Payload-Specific Feedback (PLI, REMB 등)
pub const RTCP_PT_PSFB: u8 = 206;
/// PLI feedback message type (FMT=1)
pub const RTCP_FMT_PLI: u8 = 1;
/// REMB feedback message type (FMT=15)
pub const RTCP_FMT_REMB: u8 = 15;

// --- REMB (Server-generated) ---
/// 서버 자체 REMB 전송 주기 (ms) — publisher에게 대역폭 힘트 제공
pub const REMB_INTERVAL_MS: u64 = 1_000;
/// 서버 REMB 권장 비트레이트 (bps) — Chrome BWE의 상한 힌트
/// .env `REMB_BITRATE_BPS=500000` 으로 오버라이드 가능
pub const REMB_BITRATE_BPS: u64 = 500_000;

/// .env REMB_BITRATE_BPS 파싱 (기본 500kbps)
pub fn resolve_remb_bitrate() -> u64 {
    std::env::var("REMB_BITRATE_BPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(REMB_BITRATE_BPS)
}

// --- BWE (Bandwidth Estimation) Mode ---
/// 대역폭 추정 모드: TWCC(적응적) 또는 REMB(고정 힌트)
/// .env `BWE_MODE=twcc` 또는 `BWE_MODE=remb` (기본: twcc)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BweMode {
    Twcc,
    Remb,
}

impl std::fmt::Display for BweMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BweMode::Twcc => write!(f, "twcc"),
            BweMode::Remb => write!(f, "remb"),
        }
    }
}

/// .env BWE_MODE 파싱 (기본 twcc)
pub fn resolve_bwe_mode() -> BweMode {
    match std::env::var("BWE_MODE").unwrap_or_default().to_lowercase().as_str() {
        "remb" => BweMode::Remb,
        _ => BweMode::Twcc,
    }
}

// --- TWCC (Transport-Wide Congestion Control) ---
/// TWCC RTP header extension ID (서버 extmap 정책과 일치해야 함)
pub const TWCC_EXTMAP_ID: u8 = 6;
/// TwccRecorder 링버퍼 크기 (twcc_seq % SIZE 인덱싱)
/// 약 4초분 @2000pps. 128KB per participant.
pub const TWCC_RECORDER_CAPACITY: usize = 8192;
/// TWCC feedback RTCP payload type (RFC 8888 이전 draft 기반, Chrome 호환)
pub const RTCP_PT_RTPFB: u8 = 205;
/// TWCC feedback message type (FMT=15)
pub const RTCP_FMT_TWCC: u8 = 15;
/// TWCC feedback 전송 주기 (ms)
pub const TWCC_FEEDBACK_INTERVAL_MS: u64 = 100;

// --- Floor Control (MCPTT/MBCP) ---
/// Room 모드
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoomMode {
    /// 일반 화상회의 (동시 발화 가능)
    Conference,
    /// 무전기 모드 (Floor Control, 1인 발화)
    Ptt,
}

impl std::fmt::Display for RoomMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RoomMode::Conference => write!(f, "conference"),
            RoomMode::Ptt => write!(f, "ptt"),
        }
    }
}

/// T2: 최대 Talk Burst 시간 (ms) — 초과 시 서버가 Floor Revoke
pub const FLOOR_MAX_BURST_MS: u64 = 30_000;
/// T_FLOOR_PING: 발화자 생존 확인 주기 (ms) — 클라이언트가 전송
pub const FLOOR_PING_INTERVAL_MS: u64 = 2_000;
/// T_FLOOR_TIMEOUT: Floor PING 미수신 시 revoke (ms)
pub const FLOOR_PING_TIMEOUT_MS: u64 = 5_000;

// --- Debug ---
/// RTP/RELAY hot-path: 상세 로그 출력 패킷 수 (이후 SUMMARY_INTERVAL마다 요약)
pub const DBG_DETAIL_LIMIT: u64 = 50;
/// 요약 로그 주기 (패킷 수 기준)
pub const DBG_SUMMARY_INTERVAL: u64 = 1000;
