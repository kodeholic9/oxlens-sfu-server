// author: kodeholic (powered by Claude)
//! Participant — per-user state with 2PC (publish + subscribe) sessions
//!
//! 2PC 구조:
//!   - publish_session:  클라이언트 → 서버 (recvonly on server side)
//!     ICE/DTLS/SRTP 1세트, 내 트랙 송신용, 거의 불변
//!   - subscribe_session: 서버 → 클라이언트 (sendonly on server side)
//!     ICE/DTLS/SRTP 1세트, 다른 참가자 트랙 수신용, re-nego 대상
//!
//! 서버는 ufrag를 PC별로 2개 생성하여 STUN latch 시 PC 종류를 식별한다.
//! latching 후에는 sockaddr → (user_id, PcType) 매핑으로 O(1) 식별.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU8, AtomicU16, AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;
use tokio::sync::mpsc;
use tracing::trace;

use std::collections::HashMap;
use crate::config;
use crate::transport::srtp::SrtpContext;
use crate::transport::udp::twcc::TwccRecorder;
use crate::transport::udp::rtcp_terminator::{RecvStats, SendStats};

// ============================================================================
// PipelineStats — per-participant 파이프라인 카운터 (AI 진단용)
// ============================================================================

/// 참가자별 미디어 파이프라인 통과량 카운터.
/// 전부 AtomicU64 — 핫패스에서 fetch_add(1, Relaxed) ~1ns.
/// flush 시 load(Relaxed)로 누적값 읽기 (swap 안 함, counter 타입).
/// delta 계산은 어드민 JS에서 "현재값 - 이전값"으로 처리.
pub struct PipelineStats {
    // --- Publisher 관점 (내가 보낸 것) ---
    /// ingress 수신 성공 (decrypt 후, RTX 포함)
    pub pub_rtp_in:        AtomicU64,
    /// PTT gate에서 차단된 RTP
    pub pub_rtp_gated:     AtomicU64,
    /// rewriter 통과 (PTT 모드에서 리라이팅 성공)
    pub pub_rtp_rewritten: AtomicU64,
    /// 키프레임 대기 중 드롭 (PTT 비디오)
    pub pub_video_pending: AtomicU64,

    // --- Subscriber 관점 (내가 받은 것) ---
    /// egress 큐 전달 성공
    pub sub_rtp_relayed:   AtomicU64,
    /// egress 큐 full로 드롭
    pub sub_rtp_dropped:   AtomicU64,
    /// 이 subscriber에게 보낸 SR 수
    pub sub_sr_relayed:    AtomicU64,
}

impl PipelineStats {
    pub fn new() -> Self {
        Self {
            pub_rtp_in:        AtomicU64::new(0),
            pub_rtp_gated:     AtomicU64::new(0),
            pub_rtp_rewritten: AtomicU64::new(0),
            pub_video_pending: AtomicU64::new(0),
            sub_rtp_relayed:   AtomicU64::new(0),
            sub_rtp_dropped:   AtomicU64::new(0),
            sub_sr_relayed:    AtomicU64::new(0),
        }
    }

    /// 누적값 스냅샷 (counter 타입 — swap 안 함)
    pub fn snapshot(&self) -> PipelineSnapshot {
        PipelineSnapshot {
            pub_rtp_in:        self.pub_rtp_in.load(Ordering::Relaxed),
            pub_rtp_gated:     self.pub_rtp_gated.load(Ordering::Relaxed),
            pub_rtp_rewritten: self.pub_rtp_rewritten.load(Ordering::Relaxed),
            pub_video_pending: self.pub_video_pending.load(Ordering::Relaxed),
            sub_rtp_relayed:   self.sub_rtp_relayed.load(Ordering::Relaxed),
            sub_rtp_dropped:   self.sub_rtp_dropped.load(Ordering::Relaxed),
            sub_sr_relayed:    self.sub_sr_relayed.load(Ordering::Relaxed),
        }
    }
}

impl Default for PipelineStats {
    fn default() -> Self { Self::new() }
}

/// PipelineStats의 순간 스냅샷 (plain values, JSON 직렬화용)
pub struct PipelineSnapshot {
    pub pub_rtp_in:        u64,
    pub pub_rtp_gated:     u64,
    pub pub_rtp_rewritten: u64,
    pub pub_video_pending: u64,
    pub sub_rtp_relayed:   u64,
    pub sub_rtp_dropped:   u64,
    pub sub_sr_relayed:    u64,
}

impl PipelineSnapshot {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "pub_rtp_in":        self.pub_rtp_in,
            "pub_rtp_gated":     self.pub_rtp_gated,
            "pub_rtp_rewritten": self.pub_rtp_rewritten,
            "pub_video_pending": self.pub_video_pending,
            "sub_rtp_relayed":   self.sub_rtp_relayed,
            "sub_rtp_dropped":   self.sub_rtp_dropped,
            "sub_sr_relayed":    self.sub_sr_relayed,
        })
    }
}

// ============================================================================
// SimulcastRewriter — subscriber별 가상 SSRC rewrite (Phase 3)
// ============================================================================

/// Simulcast 레이어 전환 시 SSRC/seq/ts를 가상 값으로 rewrite.
/// subscriber는 항상 단일 virtual_ssrc만 보므로 Chrome SDP 매칭 보장.
///
/// 시맨틱:
///   initialized=false + P-frame  → Drop (키프레임부터 시작)
///   initialized=false + 키프레임 → offset=0, initialized=true → Pass
///   pending_keyframe  + P-frame  → Drop
///   pending_keyframe  + 키프레임 → offset 재계산 → Pass
///   normal            → SSRC/seq/ts rewrite → Pass
pub struct SimulcastRewriter {
    pub virtual_ssrc: u32,
    seq_offset: u16,
    ts_offset: u32,
    last_out_seq: u16,
    last_out_ts: u32,
    pub pending_keyframe: bool,
    pub initialized: bool,
}

impl SimulcastRewriter {
    pub fn new(virtual_ssrc: u32) -> Self {
        Self {
            virtual_ssrc,
            seq_offset: 0,
            ts_offset: 0,
            last_out_seq: 0,
            last_out_ts: 0,
            pending_keyframe: false,
            initialized: false,
        }
    }

    /// 레이어 전환: 키프레임 도착까지 모든 패킷 드롭
    pub fn switch_layer(&mut self) {
        self.pending_keyframe = true;
    }

    /// RTP 패킷 rewrite. buf를 in-place 수정 (SSRC/seq/ts).
    /// Returns true if packet should be forwarded, false if dropped.
    pub fn rewrite(&mut self, buf: &mut [u8], is_keyframe: bool) -> bool {
        if buf.len() < 12 { return false; }

        let input_seq = u16::from_be_bytes([buf[2], buf[3]]);
        let input_ts = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);

        if !self.initialized {
            if !is_keyframe { return false; }
            // 첫 키프레임: offset 0으로 시작
            self.seq_offset = 0;
            self.ts_offset = 0;
            self.initialized = true;
            self.pending_keyframe = false;
        } else if self.pending_keyframe {
            if !is_keyframe { return false; }
            // 레이어 전환 키프레임: seq/ts 연속 보장을 위한 offset 재계산
            let target_seq = self.last_out_seq.wrapping_add(1);
            self.seq_offset = target_seq.wrapping_sub(input_seq);
            let target_ts = self.last_out_ts.wrapping_add(1);
            self.ts_offset = target_ts.wrapping_sub(input_ts);
            self.pending_keyframe = false;
        }

        // Apply offsets
        let out_seq = input_seq.wrapping_add(self.seq_offset);
        let out_ts = input_ts.wrapping_add(self.ts_offset);

        // Write virtual SSRC
        buf[8..12].copy_from_slice(&self.virtual_ssrc.to_be_bytes());
        // Write rewritten seq
        buf[2..4].copy_from_slice(&out_seq.to_be_bytes());
        // Write rewritten ts
        buf[4..8].copy_from_slice(&out_ts.to_be_bytes());

        self.last_out_seq = out_seq;
        self.last_out_ts = out_ts;

        true
    }

    /// 가상 seq → 실제 seq 역매핑 (NACK용, best-effort)
    pub fn reverse_seq(&self, virtual_seq: u16) -> u16 {
        virtual_seq.wrapping_sub(self.seq_offset)
    }
}

/// Subscriber별 특정 publisher에 대한 레이어 구독 상태
pub struct SubscribeLayerEntry {
    /// 구독 중인 레이어: "h", "l", "pause"
    pub rid: String,
    /// 가상 SSRC rewriter
    pub rewriter: SimulcastRewriter,
}

// ============================================================================
// EgressPacket — subscriber egress task에 전달할 plaintext 패킷
// ============================================================================

/// Egress task가 encrypt → send하는 plaintext 패킷 종류
pub enum EgressPacket {
    /// RTP plaintext (fan-out 미디어)
    Rtp(Vec<u8>),
    /// RTCP plaintext (SR relay 등)
    Rtcp(Vec<u8>),
}

// ============================================================================
// PcType — PeerConnection 종류 식별
// ============================================================================

/// 2PC 구조에서 PeerConnection 종류를 식별하는 enum.
/// STUN latch 시 서버 ufrag로 판별하며, sockaddr_map에 저장된다.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PcType {
    /// 클라이언트 → 서버 (서버가 recvonly, 미디어 수신)
    Publish,
    /// 서버 → 클라이언트 (서버가 sendonly, 미디어 전송)
    Subscribe,
}

impl std::fmt::Display for PcType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PcType::Publish => write!(f, "pub"),
            PcType::Subscribe => write!(f, "sub"),
        }
    }
}

// ============================================================================
// Track
// ============================================================================

#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub enum TrackKind {
    Audio,
    Video,
}

impl std::fmt::Display for TrackKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TrackKind::Audio => write!(f, "audio"),
            TrackKind::Video => write!(f, "video"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Track {
    pub ssrc: u32,
    pub kind: TrackKind,
    pub track_id: String,
    /// RTX SSRC (video only, RFC 4588)
    pub rtx_ssrc: Option<u32>,
    /// mute 상태 (true = 송신 중단)
    pub muted: bool,
    /// Simulcast RTP stream ID ("h" | "l", None for non-simulcast)
    pub rid: Option<String>,
    /// Simulcast 그룹 ID (같은 소스의 h/l 레이어는 동일 group)
    pub simulcast_group: Option<u32>,
}

// ============================================================================
// MediaSession — ICE/DTLS/SRTP 세션 (PC당 1개)
// ============================================================================

/// 하나의 PeerConnection에 대응하는 미디어 전송 세션.
/// publish/subscribe 각각 독립된 ICE/DTLS/SRTP 상태를 가진다.
pub struct MediaSession {
    // --- ICE ---
    pub ufrag:   String,
    pub ice_pwd: String,

    // --- transport ---
    /// Latched UDP address (STUN Binding Request 성공 시 설정)
    pub address: Mutex<Option<SocketAddr>>,

    // --- SRTP ---
    pub inbound_srtp:  Mutex<SrtpContext>,
    pub outbound_srtp: Mutex<SrtpContext>,
}

impl MediaSession {
    pub fn new(ufrag: String, ice_pwd: String) -> Self {
        Self {
            ufrag,
            ice_pwd,
            address:       Mutex::new(None),
            inbound_srtp:  Mutex::new(SrtpContext::new()),
            outbound_srtp: Mutex::new(SrtpContext::new()),
        }
    }

    /// STUN latch: 확인된 UDP 주소 설정
    pub fn latch_address(&self, addr: SocketAddr) {
        *self.address.lock().unwrap() = Some(addr);
    }

    pub fn get_address(&self) -> Option<SocketAddr> {
        *self.address.lock().unwrap()
    }

    /// SRTP 키 설치 여부 (DTLS 핸드셰이크 완료 = 미디어 준비)
    pub fn is_media_ready(&self) -> bool {
        self.inbound_srtp.lock().unwrap().is_ready()
    }

    /// DTLS 핸드셰이크 완료 후 SRTP 키 설치
    pub fn install_srtp_keys(
        &self,
        client_key:  &[u8],
        client_salt: &[u8],
        server_key:  &[u8],
        server_salt: &[u8],
    ) {
        self.inbound_srtp.lock().unwrap().install_key(client_key, client_salt);
        self.outbound_srtp.lock().unwrap().install_key(server_key, server_salt);
    }
}

// ============================================================================
// RtpCache — 비디오 RTP 링버퍼 캐시 (NACK → RTX 재전송용)
// ============================================================================

/// Publisher의 비디오 RTP plaintext를 캐시.
/// subscriber가 NACK을 보내면 캐시에서 찾아 RTX로 재전송한다.
/// 고정 크기 링버퍼, key = seq % SIZE. 오래된 패킷은 자연 덮어쓰기.
pub struct RtpCache {
    slots: Vec<Option<Vec<u8>>>,
}

impl RtpCache {
    pub fn new() -> Self {
        let mut slots = Vec::with_capacity(config::RTP_CACHE_SIZE);
        slots.resize_with(config::RTP_CACHE_SIZE, || None);
        Self { slots }
    }

    /// RTP plaintext 저장 (seq로 인덱싱)
    pub fn store(&mut self, seq: u16, plaintext: &[u8]) {
        let idx = (seq as usize) % config::RTP_CACHE_SIZE;
        self.slots[idx] = Some(plaintext.to_vec());
    }

    /// 진단용: 슬롯에 저장된 seq 확인 (None=비어있음, Some(seq)=다른 seq 점유)
    pub fn slot_seq(&self, seq: u16) -> Option<u16> {
        let idx = (seq as usize) % config::RTP_CACHE_SIZE;
        self.slots[idx].as_ref().and_then(|pkt| {
            if pkt.len() >= config::RTP_HEADER_MIN_SIZE {
                Some(u16::from_be_bytes([pkt[2], pkt[3]]))
            } else {
                None
            }
        })
    }

    /// seq로 캐시된 RTP 조회
    pub fn get(&self, seq: u16) -> Option<&[u8]> {
        let idx = (seq as usize) % config::RTP_CACHE_SIZE;
        self.slots[idx].as_ref().and_then(|pkt| {
            // seq 검증: 캐시된 패킷의 seq가 요청한 seq와 일치하는지 확인
            if pkt.len() >= config::RTP_HEADER_MIN_SIZE {
                let cached_seq = u16::from_be_bytes([pkt[2], pkt[3]]);
                if cached_seq == seq {
                    return Some(pkt.as_slice());
                }
            }
            None
        })
    }
}

impl Default for RtpCache {
    fn default() -> Self { Self::new() }
}

// ============================================================================
// Participant — 2PC 세션 소유
// ============================================================================

pub struct Participant {
    // --- identity ---
    pub user_id:    String,
    pub room_id:    String,
    pub joined_at:  u64,
    pub last_seen:  AtomicU64,

    // --- signaling ---
    /// WebSocket으로 JSON 메시지 전송
    pub ws_tx: mpsc::UnboundedSender<String>,

    // --- 2PC sessions ---
    /// 클라이언트 → 서버 (내 미디어 송신)
    pub publish:   MediaSession,
    /// 서버 → 클라이언트 (다른 참가자 미디어 수신)
    pub subscribe: MediaSession,

    // --- tracks ---
    /// 이 참가자가 publish하는 트랙 목록 (publish_tracks 메시지로 등록)
    pub tracks: Mutex<Vec<Track>>,

    // --- RTX (RFC 4588) ---
    /// 비디오 RTP 캐시 (NACK → RTX 재전송용)
    pub rtp_cache: Mutex<RtpCache>,
    /// RTX SSRC 할당용 카운터 (참가자별 고유)
    rtx_ssrc_counter: AtomicU32,
    /// RTX 패킷 전용 seq 카운터 (subscriber별이 아닌 publisher별)
    pub rtx_seq: AtomicU16,

    // --- TWCC (Transport-Wide Congestion Control) ---
    /// Publisher RTP의 twcc seq → 도착 시간 기록 (feedback 생성용)
    pub twcc_recorder: Mutex<TwccRecorder>,

    // --- RTCP Terminator (서버 자체 RR/SR 생성) ---
    /// Publisher SSRC별 수신 통계 (서버가 peer로서 RR 생성용)
    /// key = media SSRC, value = RecvStats
    pub recv_stats: Mutex<HashMap<u32, RecvStats>>,
    /// Subscriber 방향 송신 통계 (서버가 peer로서 SR 생성용)
    /// key = 송신 SSRC (conference: 원본, PTT: 가상), value = SendStats
    pub send_stats: Mutex<HashMap<u32, SendStats>>,

    // --- Egress (Phase W-3: subscriber별 egress task) ---
    /// subscribe PC egress channel — plaintext를 egress task에 전달
    pub egress_tx: mpsc::Sender<EgressPacket>,
    /// egress task spawn 시 .take()으로 꼼냄 (1회용)
    pub egress_rx: Mutex<Option<mpsc::Receiver<EgressPacket>>>,

    // --- RTX budget (per-subscriber, 3s window) ---
    /// 이 subscriber에게 보낸 RTX 패킷 수 (현재 3s 윈도우). flush_metrics에서 3초마다 reset.
    pub rtx_budget_used: AtomicU64,

    // --- PLI burst cancel (Phase M-1) ---
    /// 진행 중인 PLI burst task의 AbortHandle (참가자 퇴장 시 cancel)
    pub pli_burst_handle: Mutex<Option<tokio::task::AbortHandle>>,

    // --- Audio diagnostics ---
    /// 마지막 audio RTP 도착 시각 (µs, Instant 대신 u64로 저장 — AtomicU64 호환)
    pub last_audio_arrival_us: AtomicU64,

    // --- Simulcast ---
    /// Chrome offerer가 할당한 TWCC extmap ID (client-offer 모드에서 전달받음)
    pub twcc_extmap_id: AtomicU8,
    /// Publisher별 고정 가상 video SSRC (simulcast 전용, 0=미할당)
    pub simulcast_video_ssrc: AtomicU32,
    /// Subscriber별 레이어 구독 상태 (key = publisher user_id)
    pub subscribe_layers: Mutex<HashMap<String, SubscribeLayerEntry>>,

    // --- Pipeline Stats (per-participant AI 진단용) ---
    /// 파이프라인 구간별 통과량 카운터 (counter 타입, 누적)
    pub pipeline: PipelineStats,
}

impl Participant {
    pub fn new(
        user_id:    String,
        room_id:    String,
        pub_ufrag:  String,
        pub_pwd:    String,
        sub_ufrag:  String,
        sub_pwd:    String,
        ws_tx:      mpsc::UnboundedSender<String>,
        joined_at:  u64,
    ) -> Self {
        trace!(
            "Participant::new user={} room={} pub_ufrag={} sub_ufrag={}",
            user_id, room_id, pub_ufrag, sub_ufrag
        );
        let (egress_tx, egress_rx) = mpsc::channel(config::EGRESS_QUEUE_SIZE);
        Self {
            user_id,
            room_id,
            joined_at,
            last_seen:  AtomicU64::new(joined_at),
            ws_tx,
            publish:    MediaSession::new(pub_ufrag, pub_pwd),
            subscribe:  MediaSession::new(sub_ufrag, sub_pwd),
            tracks:     Mutex::new(Vec::new()),
            rtp_cache:  Mutex::new(RtpCache::new()),
            twcc_recorder: Mutex::new(TwccRecorder::new()),
            recv_stats: Mutex::new(HashMap::new()),
            send_stats: Mutex::new(HashMap::new()),
            rtx_ssrc_counter: AtomicU32::new(0),
            rtx_seq:    AtomicU16::new(0),
            egress_tx,
            egress_rx:  Mutex::new(Some(egress_rx)),
            rtx_budget_used: AtomicU64::new(0),
            pli_burst_handle: Mutex::new(None),
            last_audio_arrival_us: AtomicU64::new(0),
            twcc_extmap_id: AtomicU8::new(0),
            simulcast_video_ssrc: AtomicU32::new(0),
            subscribe_layers: Mutex::new(HashMap::new()),
            pipeline: PipelineStats::new(),
        }
    }

    pub fn touch(&self, ts: u64) {
        self.last_seen.store(ts, Ordering::Relaxed);
    }

    /// PcType에 해당하는 MediaSession 참조
    pub fn session(&self, pc: PcType) -> &MediaSession {
        match pc {
            PcType::Publish   => &self.publish,
            PcType::Subscribe => &self.subscribe,
        }
    }

    /// 트랙 등록 (SSRC 중복 방지). video 트랙은 RTX SSRC 자동 할당.
    pub fn add_track(&self, ssrc: u32, kind: TrackKind, track_id: String) {
        let mut tracks = self.tracks.lock().unwrap();
        if !tracks.iter().any(|t| t.ssrc == ssrc) {
            let rtx_ssrc = if kind == TrackKind::Video {
                Some(self.alloc_rtx_ssrc(ssrc))
            } else {
                None
            };
            tracks.push(Track { ssrc, kind, track_id, rtx_ssrc, muted: false, rid: None, simulcast_group: None });
            trace!("track added ssrc={} rtx_ssrc={:?} user={}", ssrc, rtx_ssrc, self.user_id);
        }
    }

    /// Simulcast rid/simulcast_group 포함 트랙 등록
    pub fn add_track_ext(&self, ssrc: u32, kind: TrackKind, track_id: String, rid: Option<String>, simulcast_group: Option<u32>) {
        let mut tracks = self.tracks.lock().unwrap();
        if !tracks.iter().any(|t| t.ssrc == ssrc) {
            let rtx_ssrc = if kind == TrackKind::Video {
                Some(self.alloc_rtx_ssrc(ssrc))
            } else {
                None
            };
            tracks.push(Track { ssrc, kind, track_id, rtx_ssrc, muted: false, rid, simulcast_group });
            trace!("track added (ext) ssrc={} rtx_ssrc={:?} rid={:?} group={:?} user={}",
                ssrc, rtx_ssrc, tracks.last().unwrap().rid, simulcast_group, self.user_id);
        }
    }

    /// RTX SSRC 할당: media_ssrc + 1000 + counter (충돌 회피)
    fn alloc_rtx_ssrc(&self, media_ssrc: u32) -> u32 {
        let offset = self.rtx_ssrc_counter.fetch_add(1, Ordering::Relaxed);
        media_ssrc.wrapping_add(1000).wrapping_add(offset)
    }

    /// 다음 RTX seq 번호 발급
    pub fn next_rtx_seq(&self) -> u16 {
        self.rtx_seq.fetch_add(1, Ordering::Relaxed)
    }

    /// 트랙 제거 (SSRC 기준)
    pub fn remove_track(&self, ssrc: u32) -> Option<Track> {
        let mut tracks = self.tracks.lock().unwrap();
        if let Some(pos) = tracks.iter().position(|t| t.ssrc == ssrc) {
            Some(tracks.remove(pos))
        } else {
            None
        }
    }

    /// 트랙 mute 상태 변경. 성공 시 해당 트랙의 TrackKind 반환.
    pub fn set_track_muted(&self, ssrc: u32, muted: bool) -> Option<TrackKind> {
        let mut tracks = self.tracks.lock().unwrap();
        if let Some(track) = tracks.iter_mut().find(|t| t.ssrc == ssrc) {
            track.muted = muted;
            Some(track.kind.clone())
        } else {
            None
        }
    }

    /// 현재 publish 트랙 목록 스냅샷
    pub fn get_tracks(&self) -> Vec<Track> {
        self.tracks.lock().unwrap().clone()
    }

    // --- 편의 메서드: publish session 기준 ---

    /// publish PC가 미디어 준비 완료인지
    pub fn is_publish_ready(&self) -> bool {
        self.publish.is_media_ready()
    }

    /// subscribe PC가 미디어 준비 완료인지
    pub fn is_subscribe_ready(&self) -> bool {
        self.subscribe.is_media_ready()
    }

    /// 진행 중인 PLI burst task cancel
    pub fn cancel_pli_burst(&self) {
        if let Some(handle) = self.pli_burst_handle.lock().unwrap().take() {
            handle.abort();
        }
    }

    /// Simulcast 가상 video SSRC lazy 할당 (CAS, 한번 할당되면 고정)
    pub fn ensure_simulcast_video_ssrc(&self) -> u32 {
        let existing = self.simulcast_video_ssrc.load(Ordering::Relaxed);
        if existing != 0 { return existing; }
        let new_ssrc = rand_u32_nonzero();
        match self.simulcast_video_ssrc.compare_exchange(
            0, new_ssrc, Ordering::SeqCst, Ordering::Relaxed
        ) {
            Ok(_) => new_ssrc,
            Err(winner) => winner,
        }
    }
}

/// 0이 아닌 랜덤 u32 생성 (SSRC 할당용)
fn rand_u32_nonzero() -> u32 {
    loop {
        let mut buf = [0u8; 4];
        getrandom::fill(&mut buf).expect("getrandom failed");
        let v = u32::from_le_bytes(buf);
        if v != 0 { return v; }
    }
}
