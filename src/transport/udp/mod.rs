// author: kodeholic (powered by Claude)
//! UDP media transport — 2PC structure, single port, demux dispatch
//!
//! 2PC 구조에서의 패킷 흐름:
//!   recv_from(addr)
//!     → classify (RFC 5764 first-byte)
//!     → STUN : RoomHub.latch_by_ufrag() → (Participant, PcType) → Binding Response → trigger DTLS
//!     → DTLS : DtlsSessionMap.inject() or start new handshake (per PC session)
//!     → SRTP : RoomHub.find_by_addr() → (Participant, PcType)
//!              PcType::Publish  → decrypt → relay → encrypt → send to subscribers
//!              PcType::Subscribe → NACK(RTX) + RTCP relay(RR/PLI/REMB)
//!
//! 모듈 구조:
//!   mod.rs      — UdpTransport 본체 + run() + STUN + DTLS + worker
//!   ingress.rs  — publish RTP/RTCP 수신 처리 (hot path)
//!   egress.rs   — subscriber 방향 송신 (egress task, PLI, REMB)
//!   rtcp.rs     — RTCP/RTP 패킷 파싱 및 조립 헬퍼
//!   (metrics → src/metrics/ 모듈로 분리, v0.4.0)

mod rtcp;
pub(crate) mod rtcp_terminator;
mod ingress;
mod egress;
pub(crate) mod twcc;
pub(crate) mod pli;

// Re-exports for external use
pub use rtcp::build_pli;
pub use pli::spawn_pli_burst;

use bytes::{Bytes, BytesMut};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use tokio::net::UdpSocket;
use tracing::{debug, error, info, trace, warn};
// 로그 레벨 정책:
//   info!  = 운영 필수 (연결/해제, 에러, 상태 전환)
//   debug! = 진단용 상세 (패킷 단위 로그 앞 50건)
//   trace! = 패킷 레벨 상세 (요약 로그 포함)

use webrtc_util::conn::Conn;

use crate::config;
use crate::config::BweMode;
use crate::metrics::GlobalMetrics;
use crate::room::participant::PcType;
use crate::room::room::RoomHub;
use crate::transport::demux::{self, PacketType};
use crate::transport::demux_conn::{DemuxConn, DtlsPacketTx};
use crate::transport::dtls::{self, ServerCert};
use crate::transport::stun;

use rtcp::current_ts;
use egress::{run_egress_task, send_pli_to_publishers};

// ============================================================================
// DTLS Session Map (addr → packet channel)
// ============================================================================

struct DtlsSessionMap {
    sessions: HashMap<SocketAddr, DtlsPacketTx>,
}

impl DtlsSessionMap {
    fn new() -> Self {
        Self { sessions: HashMap::new() }
    }

    fn insert(&mut self, addr: SocketAddr, tx: DtlsPacketTx) {
        self.sessions.insert(addr, tx);
    }

    async fn inject(&self, addr: &SocketAddr, data: Bytes) -> bool {
        if let Some(tx) = self.sessions.get(addr) {
            tx.send(data).await.is_ok()
        } else {
            false
        }
    }

    fn has(&self, addr: &SocketAddr) -> bool {
        self.sessions.contains_key(addr)
    }

    fn remove_stale(&mut self) {
        self.sessions.retain(|addr, tx| {
            if tx.is_closed() {
                debug!("stale DTLS session removed addr={}", addr);
                false
            } else {
                true
            }
        });
    }
}

// ============================================================================
// UdpTransport
// ============================================================================

pub struct UdpTransport {
    pub socket:                 Arc<UdpSocket>,
    pub(crate) room_hub:       Arc<RoomHub>,
    pub(crate) cert:           Arc<ServerCert>,
    dtls_map:                  DtlsSessionMap,
    pub(crate) pkt_count:      u64,
    pub(crate) dbg_rtp_count:  AtomicU64,
    pub(crate) metrics:        Arc<GlobalMetrics>,
    // Phase W-2: multi-worker
    pub(crate) worker_id: u8,
    // BWE mode (TWCC or REMB)
    pub(crate) bwe_mode: BweMode,
    // REMB bitrate (bps, .env REMB_BITRATE_BPS)
    pub(crate) remb_bitrate: u64,
}

impl UdpTransport {
    pub async fn bind(
        room_hub: Arc<RoomHub>,
        cert:     Arc<ServerCert>,
    ) -> std::io::Result<Self> {
        let addr = SocketAddr::from(([0, 0, 0, 0], config::UDP_PORT));
        let socket = UdpSocket::bind(addr).await?;
        info!("UDP transport bound on {}", addr);

        let metrics = Arc::new(GlobalMetrics::new(1, &BweMode::Twcc));
        Ok(Self {
            socket: Arc::new(socket),
            room_hub,
            cert,
            dtls_map: DtlsSessionMap::new(),
            pkt_count: 0,
            dbg_rtp_count: AtomicU64::new(0),
            metrics,
            worker_id: 0,
            bwe_mode: BweMode::Twcc,
            remb_bitrate: config::REMB_BITRATE_BPS,
        })
    }

    /// 외부에서 생성된 socket을 받아 구성 (AppState와 socket 공유 시)
    #[allow(dead_code)]
    pub(crate) fn from_socket(
        socket:   Arc<UdpSocket>,
        room_hub: Arc<RoomHub>,
        cert:     Arc<ServerCert>,
        metrics:  Arc<GlobalMetrics>,
    ) -> Self {
        Self::from_socket_with_id(socket, room_hub, cert, 0, BweMode::Twcc, config::REMB_BITRATE_BPS, metrics)
    }

    /// Phase W-2: worker_id 지정 생성자
    pub(crate) fn from_socket_with_id(
        socket:   Arc<UdpSocket>,
        room_hub: Arc<RoomHub>,
        cert:     Arc<ServerCert>,
        worker_id: u8,
        bwe_mode: BweMode,
        remb_bitrate: u64,
        metrics:  Arc<GlobalMetrics>,
    ) -> Self {
        Self {
            socket,
            room_hub,
            cert,
            dtls_map: DtlsSessionMap::new(),
            pkt_count: 0,
            dbg_rtp_count: AtomicU64::new(0),
            metrics,
            worker_id,
            bwe_mode,
            remb_bitrate,
        }
    }

    pub async fn run(mut self) {
        let mut buf = BytesMut::zeroed(config::UDP_RECV_BUF_SIZE);
        let is_primary = self.worker_id == 0;

        info!("[W{}] UDP worker started (bwe={})", self.worker_id, self.bwe_mode);

        // 타이머는 worker-0만 실행 (metrics flush, REMB 전송)
        let mut metrics_timer = tokio::time::interval(
            tokio::time::Duration::from_secs(3),
        );
        metrics_timer.tick().await;

        // BWE 모드에 따라 타이머 주기 결정: TWCC=100ms, REMB=1000ms
        let bwe_interval = match self.bwe_mode {
            BweMode::Twcc => config::TWCC_FEEDBACK_INTERVAL_MS,
            BweMode::Remb => config::REMB_INTERVAL_MS,
        };
        let mut bwe_timer = tokio::time::interval(
            tokio::time::Duration::from_millis(bwe_interval),
        );
        bwe_timer.tick().await;

        // RTCP Terminator: 서버 자체 RR/SR 생성 타이머 (1초)
        let mut rtcp_report_timer = tokio::time::interval(
            tokio::time::Duration::from_millis(config::RTCP_REPORT_INTERVAL_MS),
        );
        rtcp_report_timer.tick().await;

        loop {
            tokio::select! {
                result = self.socket.recv_from(&mut buf) => {
                    let (len, remote) = match result {
                        Ok(r) => r,
                        Err(e) => { debug!("[W{}] UDP recv error: {e}", self.worker_id); continue; }
                    };

                    let data = Bytes::copy_from_slice(&buf[..len]);

                    match demux::classify(&data) {
                        PacketType::Stun => self.handle_stun(&data, remote).await,
                        PacketType::Dtls => self.handle_dtls(data, remote).await,
                        PacketType::Srtp => self.handle_srtp(&data, remote).await,
                        PacketType::Unknown => {
                            trace!("unknown packet from {} byte0=0x{:02X}", remote, data[0]);
                        }
                    }

                    self.pkt_count += 1;
                    if self.pkt_count % 1000 == 0 {
                        self.dtls_map.remove_stale();
                    }
                }
                _ = metrics_timer.tick(), if is_primary => {
                    self.flush_metrics();
                }
                _ = bwe_timer.tick(), if is_primary => {
                    match self.bwe_mode {
                        BweMode::Twcc => self.send_twcc_to_publishers().await,
                        BweMode::Remb => self.send_remb_to_publishers().await,
                    }
                }
                _ = rtcp_report_timer.tick(), if is_primary => {
                    self.send_rtcp_reports().await;
                }
            }
        }
    }

    /// 3초마다 metrics 집계 → admin_tx로 push (GlobalMetrics가 swap+reset 통합 처리)
    fn flush_metrics(&self) {
        // Per-subscriber RTX budget 리셋 (3s window)
        for room_entry in self.room_hub.rooms.iter() {
            for entry in room_entry.value().participants.iter() {
                entry.value().rtx_budget_used.swap(0, std::sync::atomic::Ordering::Relaxed);
            }
        }

        let mut json = self.metrics.flush();

        // RTCP Terminator 진단: rr_diag 콘솔 출력 (안정화 완료 → debug 레벨)
        if let Some(diag) = json.get("rr_diag").and_then(|v| v.as_array()) {
            if !diag.is_empty() {
                debug!("[RTCP:TERM:DIAG] rr_diag ({} blocks):", diag.len());
                for entry in diag {
                    debug!("  {}", entry);
                }
            }
        }

        // Per-participant pipeline stats (counter 타입, 누적값 스냅샷)
        // delta 계산은 어드민 JS에서 "현재값 - 이전값"으로 처리
        let mut pipeline_map = serde_json::Map::new();
        for room_entry in self.room_hub.rooms.iter() {
            let room = room_entry.value();

            // 활성 세션 종료 감지: 빈 방 + active_since != 0 → 리셋
            if room.participants.is_empty() {
                let prev = room.active_since.swap(0, std::sync::atomic::Ordering::Relaxed);
                if prev != 0 {
                    crate::agg_logger::clear_room(&room.id);
                    debug!("[METRICS] room {} active session ended (was since {})", room.id, prev);
                }
                continue;
            }

            let active_since = room.active_since.load(std::sync::atomic::Ordering::Relaxed);
            let mut participants_map = serde_json::Map::new();
            // 방 활성 세션 시작 시각
            participants_map.insert("_active_since".to_string(), serde_json::json!(active_since));
            for entry in room.participants.iter() {
                let p = entry.value();
                let snap = p.pipeline.snapshot();
                let mut p_json = snap.to_json();
                if let Some(obj) = p_json.as_object_mut() {
                    obj.insert("since".to_string(), serde_json::json!(p.joined_at));
                }
                participants_map.insert(p.user_id.clone(), p_json);
            }
            pipeline_map.insert(room.id.clone(), serde_json::Value::Object(participants_map));
        }
        if !pipeline_map.is_empty() {
            if let Some(root) = json.as_object_mut() {
                root.insert("pipeline".to_string(), serde_json::Value::Object(pipeline_map));
            }
        }

        crate::telemetry_bus::emit(
            crate::telemetry_bus::TelemetryEvent::ServerMetrics(json)
        );

        // AggLogger flush (같은 3초 tick에서 처리)
        crate::agg_logger::flush();
    }

    // ========================================================================
    // STUN — cold path (ICE connectivity check)
    // ========================================================================

    async fn handle_stun(&mut self, buf: &[u8], remote: SocketAddr) {
        let msg = match stun::parse(buf) {
            Some(m) => m,
            None => { trace!("STUN parse failed from {}", remote); return; }
        };

        if msg.msg_type != stun::BINDING_REQUEST {
            trace!("non-binding STUN from {} type=0x{:04X}", remote, msg.msg_type);
            return;
        }

        let username = match msg.username() {
            Some(u) => u,
            None => { debug!("STUN without USERNAME from {}", remote); return; }
        };
        let server_ufrag = match username.split(':').next() {
            Some(s) => s,
            None => { debug!("invalid STUN USERNAME format: {}", username); return; }
        };

        // Latch → returns (Participant, PcType, Room)
        let (participant, pc_type, _room) = match self.room_hub.latch_by_ufrag(server_ufrag, remote) {
            Some(r) => r,
            None => { debug!("unknown ufrag={} from {}", server_ufrag, remote); return; }
        };

        participant.touch(current_ts());

        let session = participant.session(pc_type);

        info!("[DBG:STUN] latch user={} pc={} ufrag={} addr={}",
            participant.user_id, pc_type, server_ufrag, remote);

        // Verify MESSAGE-INTEGRITY with the session's ice_pwd
        let integrity_key = stun::ice_integrity_key(&session.ice_pwd);
        if !stun::verify_message_integrity(&msg, &integrity_key) {
            warn!("[DBG:STUN] MESSAGE-INTEGRITY mismatch user={} pc={} addr={}",
                participant.user_id, pc_type, remote);
            return;
        }

        let response = stun::build_binding_response(
            &msg.transaction_id,
            remote,
            &integrity_key,
        );
        if let Err(e) = self.socket.send_to(&response, remote).await {
            error!("STUN response send failed: {e}");
        }
        info!("[DBG:STUN] binding-response sent user={} pc={} addr={}",
            participant.user_id, pc_type, remote);

        // USE-CANDIDATE → trigger DTLS handshake for this PC session
        if msg.has_use_candidate() && !self.dtls_map.has(&remote) {
            info!("[DBG:STUN] USE-CANDIDATE user={} pc={} addr={} → starting DTLS",
                participant.user_id, pc_type, remote);
            self.start_dtls_handshake(remote, participant, pc_type).await;
        }
    }

    // ========================================================================
    // DTLS — handshake path (per PC session)
    // ========================================================================

    async fn handle_dtls(&mut self, data: Bytes, remote: SocketAddr) {
        if self.dtls_map.inject(&remote, data.clone()).await {
            return;
        }

        let (participant, pc_type, _room) = match self.room_hub.find_by_addr(&remote) {
            Some(r) => r,
            None => {
                info!("[DBG:DTLS] from unlatched addr={}, dropping", remote);
                return;
            }
        };

        info!("[DBG:DTLS] new session user={} pc={} addr={} pkt_len={}",
            participant.user_id, pc_type, remote, data.len());
        self.start_dtls_handshake(remote, participant, pc_type).await;
        self.dtls_map.inject(&remote, data).await;
    }

    async fn start_dtls_handshake(
        &mut self,
        remote: SocketAddr,
        participant: Arc<crate::room::participant::Participant>,
        pc_type: PcType,
    ) {
        let (adapter, tx) = DemuxConn::new(Arc::clone(&self.socket), remote);
        self.dtls_map.insert(remote, tx);

        let cert = Arc::clone(&self.cert);
        let socket = Arc::clone(&self.socket);
        let room_hub = Arc::clone(&self.room_hub);
        let metrics = Arc::clone(&self.metrics);

        tokio::spawn(async move {
            info!("[DBG:DTLS] handshake starting user={} pc={} addr={}",
                participant.user_id, pc_type, remote);
            let config = dtls::server_config(&cert);
            let conn: Arc<dyn webrtc_util::conn::Conn + Send + Sync> = Arc::new(adapter);

            let timeout = tokio::time::Duration::from_secs(10);
            let result = tokio::time::timeout(timeout, dtls::accept_dtls(conn, config)).await;

            match result {
                Ok(Ok(dtls_conn)) => {
                    info!("[DBG:DTLS] handshake OK user={} pc={} addr={}",
                        participant.user_id, pc_type, remote);
                    match dtls::export_srtp_keys(&dtls_conn).await {
                        Ok(keys) => {
                            // Install SRTP keys on the specific session (publish or subscribe)
                            let session = participant.session(pc_type);
                            session.install_srtp_keys(
                                &keys.client_key,
                                &keys.client_salt,
                                &keys.server_key,
                                &keys.server_salt,
                            );
                            info!("[DBG:DTLS] SRTP ready user={} pc={} addr={}",
                                participant.user_id, pc_type, remote);

                            // Subscribe SRTP ready → egress task spawn + PLI
                            if pc_type == PcType::Subscribe {
                                // Phase W-3: subscriber별 egress task spawn
                                let egress_rx = participant.egress_rx.lock().unwrap().take();
                                if let Some(rx) = egress_rx {
                                    let eg_socket = Arc::clone(&socket);
                                    let eg_participant = Arc::clone(&participant);
                                    let eg_metrics = Arc::clone(&metrics);
                                    tokio::spawn(run_egress_task(rx, eg_participant, eg_socket, eg_metrics));
                                    info!("[EGRESS] spawned user={}", participant.user_id);
                                }

                                if let Ok(room) = room_hub.get(&participant.room_id) {
                                    info!("[DBG:PLI] subscribe ready, requesting keyframes user={}",
                                        participant.user_id);
                                    send_pli_to_publishers(&socket, &room, &participant.user_id).await;
                                }
                            }
                        }
                        Err(e) => {
                            error!("[DBG:DTLS] SRTP key export FAILED user={} pc={}: {e}",
                                participant.user_id, pc_type);
                        }
                    }

                    // Keep DTLSConn alive
                    let mut keepalive_buf = vec![0u8; 1500];
                    loop {
                        match dtls_conn.recv(&mut keepalive_buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {}
                        }
                    }
                }
                Ok(Err(e)) => {
                    warn!("[DBG:DTLS] handshake FAILED user={} pc={}: {e}",
                        participant.user_id, pc_type);
                }
                Err(_) => {
                    warn!("[DBG:DTLS] handshake TIMEOUT (10s) user={} pc={} addr={}",
                        participant.user_id, pc_type, remote);
                }
            }

            trace!("[DBG:DTLS] session ended user={} pc={} addr={}",
                participant.user_id, pc_type, remote);
        });
    }
}

// ============================================================================
// Phase W-2: SO_REUSEPORT multi-worker support (Linux only)
// ============================================================================

/// UDP worker 수 결정 (0 = auto = 코어 수)
pub fn resolve_worker_count(configured: usize) -> usize {
    if configured > 0 {
        return configured;
    }
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

/// SO_REUSEPORT로 UDP 소켓 바인드 (Linux 전용)
/// 동일 포트에 N개 소켓을 바인드하면 커널이 4-tuple hash로 패킷 분배
#[cfg(target_os = "linux")]
pub fn bind_reuseport(port: u16) -> std::io::Result<tokio::net::UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};

    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_port(true)?;
    socket.set_nonblocking(true)?;
    let addr: socket2::SockAddr = SocketAddr::from(([0, 0, 0, 0], port)).into();
    socket.bind(&addr)?;
    let std_socket: std::net::UdpSocket = socket.into();
    tokio::net::UdpSocket::from_std(std_socket)
}
