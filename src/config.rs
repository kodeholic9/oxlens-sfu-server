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

// --- Codec Payload Types (server_codec_policy와 일치해야 함) ---
pub const OPUS_PAYLOAD_TYPE: u8 = 111;
pub const VP8_PAYLOAD_TYPE: u8 = 96;
pub const VP8_RTX_PAYLOAD_TYPE: u8 = 97;
pub const H264_PAYLOAD_TYPE: u8 = 102;
pub const H264_RTX_PAYLOAD_TYPE: u8 = 103;

/// media PT → video PT 여부
pub fn is_video_pt(pt: u8) -> bool {
    pt == VP8_PAYLOAD_TYPE || pt == H264_PAYLOAD_TYPE
}

/// media PT → audio PT 여부
pub fn is_audio_pt(pt: u8) -> bool {
    pt == OPUS_PAYLOAD_TYPE
}

/// RTX PT 여부 (재전송 패킷 — recv_stats 제외 대상)
pub fn is_rtx_pt(pt: u8) -> bool {
    pt == VP8_RTX_PAYLOAD_TYPE || pt == H264_RTX_PAYLOAD_TYPE
}

/// 원본 media PT → 대응 RTX PT
pub fn rtx_pt_for(media_pt: u8) -> u8 {
    match media_pt {
        H264_PAYLOAD_TYPE => H264_RTX_PAYLOAD_TYPE,
        _ => VP8_RTX_PAYLOAD_TYPE,
    }
}

// --- RTX (RFC 4588) ---
/// RTP 캐시 링버퍼 크기 (seq % SIZE로 인덱싱, 약 4초분 @30fps)
pub const RTP_CACHE_SIZE: usize = 512;
/// RTX payload type — 하위 호환용 (신규 코드는 is_rtx_pt()/rtx_pt_for() 사용)
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

// --- RTCP APP — MBCP Floor Control over UDP (RFC 3550 Section 6.7) ---
/// RTCP APP payload type
pub const RTCP_PT_APP: u8 = 204;
/// APP subtype: Floor Request (PTT 누름)
pub const MBCP_SUBTYPE_FREQ: u8 = 0;
/// APP subtype: Floor Release (PTT 뗌)
pub const MBCP_SUBTYPE_FREL: u8 = 1;
/// APP subtype: Floor Taken (서버 → 클라이언트, 누군가 발화 획득)
pub const MBCP_SUBTYPE_FTKN: u8 = 2;
/// APP subtype: Floor Idle (서버 → 클라이언트, 발화권 해제됨)
pub const MBCP_SUBTYPE_FIDL: u8 = 3;
/// APP subtype: Floor Revoke (서버 → 클라이언트, 강제 회수)
pub const MBCP_SUBTYPE_FRVK: u8 = 4;
/// APP subtype: Floor Ping (발화자 생존 확인)
pub const MBCP_SUBTYPE_FPNG: u8 = 5;
/// APP name: "MBCP" (4 bytes ASCII, RFC 3550 APP 패킷 식별자)
pub const MBCP_APP_NAME: [u8; 4] = [b'M', b'B', b'C', b'P'];

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

// --- Floor Priority & Queuing (3GPP TS 24.380 기반) ---
/// Floor 요청 기본 우선순위 (0 = 최저, 255 = 최고)
pub const FLOOR_DEFAULT_PRIORITY: u8 = 0;
/// Floor 최대 우선순위
pub const FLOOR_MAX_PRIORITY: u8 = 255;
/// Floor 큐 최대 크기
pub const FLOOR_QUEUE_MAX_SIZE: usize = 10;
/// Granted 응답의 기본 duration (초)
pub const FLOOR_DEFAULT_DURATION_S: u16 = 30;

// --- RTX budget (per-subscriber, 3s window) ---
/// subscriber별 3초당 최대 RTX 전송 수. 초과 시 RTX를 버려서 다른 참가자 보호.
/// 정상 세션: ~10-30 RTX/3s, LTE lossy: 수백~수천. 200이면 정상은 통과, 폭풍은 차단.
pub const RTX_BUDGET_PER_3S: u64 = 200;

// --- RTCP Terminator (Server-generated RR/SR) ---
/// 서버 자체 RR/SR 생성 주기 (ms) — publisher에게 수신 품질 피드백
pub const RTCP_REPORT_INTERVAL_MS: u64 = 1_000;
/// Opus clock rate (48kHz)
pub const CLOCK_RATE_AUDIO: u32 = 48_000;
/// VP8 clock rate (90kHz)
pub const CLOCK_RATE_VIDEO: u32 = 90_000;
/// RecvStats seq validation: max dropout (reorder tolerance)
pub const RTP_SEQ_MAX_DROPOUT: u16 = 3000;
/// RecvStats seq validation: max misorder (late arrival tolerance)
pub const RTP_SEQ_MAX_MISORDER: u16 = 100;
/// RecvStats seq validation: min sequential for valid source
pub const RTP_SEQ_MIN_SEQUENTIAL: u32 = 2;

// --- Simulcast ---
/// Simulcast high 레이어 최대 비트레이트 (bps)
pub const SIMULCAST_HIGH_MAX_BITRATE: u32 = 1_650_000;
/// Simulcast low 레이어 최대 비트레이트 (bps)
pub const SIMULCAST_LOW_MAX_BITRATE: u32 = 250_000;
/// Simulcast low 레이어 해상도 축소 비율 (scaleResolutionDownBy)
pub const SIMULCAST_LOW_SCALE_DOWN: u32 = 4;

// --- Debug ---
/// RTP/RELAY hot-path: 상세 로그 출력 패킷 수 (이후 SUMMARY_INTERVAL마다 요약)
pub const DBG_DETAIL_LIMIT: u64 = 50;
/// 요약 로그 주기 (패킷 수 기준)
pub const DBG_SUMMARY_INTERVAL: u64 = 1000;
