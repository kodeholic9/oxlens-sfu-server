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

use bytes::{Bytes, BytesMut};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;
use tokio::net::UdpSocket;
use tokio::sync::broadcast;
use tracing::{debug, error, info, trace, warn};
// 로그 레벨 정책:
//   info!  = 운영 필수 (연결/해제, 에러, 상태 전환)
//   debug! = 진단용 상세 (패킷 단위 로그 앞 50건)
//   trace! = 패킷 레벨 상세 (요약 로그 포함)

use std::sync::atomic::{AtomicU64, Ordering};

use webrtc_util::conn::Conn;

use crate::config;
use crate::room::participant::PcType;
use crate::room::room::RoomHub;
use crate::transport::demux::{self, PacketType};
use crate::transport::demux_conn::{DemuxConn, DtlsPacketTx};
use crate::transport::dtls::{self, ServerCert};
use crate::transport::stun;

// ============================================================================
// Server Metrics (B구간 계측 — atomic counters + timing accumulators)
// ============================================================================

/// 3초 주기 집계를 위한 타이밍 어퀴뮬레이터
struct TimingStat {
    sum_us: u64,
    count:  u64,
    min_us: u64,
    max_us: u64,
}

impl TimingStat {
    fn new() -> Self {
        Self { sum_us: 0, count: 0, min_us: u64::MAX, max_us: 0 }
    }

    fn record(&mut self, us: u64) {
        self.sum_us += us;
        self.count += 1;
        if us < self.min_us { self.min_us = us; }
        if us > self.max_us { self.max_us = us; }
    }

    fn avg(&self) -> u64 {
        if self.count == 0 { 0 } else { self.sum_us / self.count }
    }

    #[allow(dead_code)]
    fn p95_approx(&self) -> u64 {
        // 정확한 p95는 histogram 필요. 단순 근사: max * 0.95 또는 avg + (max-avg)*0.8
        // 여기서는 max를 그대로 노출 (p95 대신 max 사용)
        self.max_us
    }

    fn to_json(&self) -> serde_json::Value {
        if self.count == 0 {
            return serde_json::json!(null);
        }
        serde_json::json!({
            "avg_us": self.avg(),
            "min_us": if self.min_us == u64::MAX { 0 } else { self.min_us },
            "max_us": self.max_us,
            "count":  self.count,
        })
    }

    fn reset(&mut self) {
        self.sum_us = 0;
        self.count = 0;
        self.min_us = u64::MAX;
        self.max_us = 0;
    }
}

struct ServerMetrics {
    // B-1: relay total (decrypt ~ last send_to)
    relay:          TimingStat,
    // B-2: SRTP decrypt
    decrypt:        TimingStat,
    // B-3: SRTP encrypt (per target)
    encrypt:        TimingStat,
    // B-4: Mutex lock wait
    lock_wait:      TimingStat,
    // B-5: fan-out count per relay
    fan_out_sum:    u64,
    fan_out_count:  u64,
    fan_out_min:    u32,
    fan_out_max:    u32,
    // B-6, B-7: encrypt/decrypt failures
    encrypt_fail:   u64,
    decrypt_fail:   u64,
    // B-8~14: RTCP counters
    nack_received:  u64,
    rtx_sent:       u64,
    rtx_cache_miss: u64,
    pli_sent:       u64,
    sr_relayed:     u64,
    rr_relayed:     u64,
    twcc_sent:      u64,
    // subscribe RTCP 진단 카운터
    sub_rtcp_received: u64,  // handle_subscribe_rtcp 진입
    sub_rtcp_not_rtcp: u64,  // is_rtcp 필터된 횟수
    sub_rtcp_decrypted: u64, // 복호화 성공
}

impl ServerMetrics {
    fn new() -> Self {
        Self {
            relay:          TimingStat::new(),
            decrypt:        TimingStat::new(),
            encrypt:        TimingStat::new(),
            lock_wait:      TimingStat::new(),
            fan_out_sum:    0,
            fan_out_count:  0,
            fan_out_min:    u32::MAX,
            fan_out_max:    0,
            encrypt_fail:   0,
            decrypt_fail:   0,
            nack_received:  0,
            rtx_sent:       0,
            rtx_cache_miss: 0,
            pli_sent:       0,
            sr_relayed:     0,
            rr_relayed:     0,
            twcc_sent:      0,
            sub_rtcp_received: 0,
            sub_rtcp_not_rtcp: 0,
            sub_rtcp_decrypted: 0,
        }
    }

    #[allow(dead_code)]
    fn record_fan_out(&mut self, count: u32) {
        self.fan_out_sum += count as u64;
        self.fan_out_count += 1;
        if count < self.fan_out_min { self.fan_out_min = count; }
        if count > self.fan_out_max { self.fan_out_max = count; }
    }

    fn to_json(&self) -> serde_json::Value {
        let fan_out_avg = if self.fan_out_count == 0 { 0.0 }
            else { self.fan_out_sum as f64 / self.fan_out_count as f64 };
        serde_json::json!({
            "type": "server_metrics",
            "relay":          self.relay.to_json(),
            "decrypt":        self.decrypt.to_json(),
            "encrypt":        self.encrypt.to_json(),
            "lock_wait":      self.lock_wait.to_json(),
            "fan_out": {
                "avg": format!("{:.1}", fan_out_avg),
                "min": if self.fan_out_min == u32::MAX { 0 } else { self.fan_out_min },
                "max": self.fan_out_max,
            },
            "encrypt_fail":   self.encrypt_fail,
            "decrypt_fail":   self.decrypt_fail,
            "nack_received":  self.nack_received,
            "rtx_sent":       self.rtx_sent,
            "rtx_cache_miss": self.rtx_cache_miss,
            "pli_sent":       self.pli_sent,
            "sr_relayed":     self.sr_relayed,
            "rr_relayed":     self.rr_relayed,
            "twcc_sent":      self.twcc_sent,
            "sub_rtcp_received": self.sub_rtcp_received,
            "sub_rtcp_not_rtcp": self.sub_rtcp_not_rtcp,
            "sub_rtcp_decrypted": self.sub_rtcp_decrypted,
        })
    }

    fn reset(&mut self) {
        self.relay.reset();
        self.decrypt.reset();
        self.encrypt.reset();
        self.lock_wait.reset();
        self.fan_out_sum = 0;
        self.fan_out_count = 0;
        self.fan_out_min = u32::MAX;
        self.fan_out_max = 0;
        self.encrypt_fail = 0;
        self.decrypt_fail = 0;
        self.nack_received = 0;
        self.rtx_sent = 0;
        self.rtx_cache_miss = 0;
        self.pli_sent = 0;
        self.sr_relayed = 0;
        self.rr_relayed = 0;
        self.twcc_sent = 0;
        self.sub_rtcp_received = 0;
        self.sub_rtcp_not_rtcp = 0;
        self.sub_rtcp_decrypted = 0;
    }
}

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
    pub socket:   Arc<UdpSocket>,
    room_hub:     Arc<RoomHub>,
    cert:         Arc<ServerCert>,
    dtls_map:     DtlsSessionMap,
    pkt_count:    u64,
    dbg_rtp_count: AtomicU64,
    metrics:      ServerMetrics,
    admin_tx:     broadcast::Sender<String>,
    // Phase W-1: spawn fan-out atomic counters
    spawn_rtp_relayed:  Arc<AtomicU64>,
    spawn_sr_relayed:   Arc<AtomicU64>,
    spawn_encrypt_fail: Arc<AtomicU64>,
}

impl UdpTransport {
    pub async fn bind(
        room_hub: Arc<RoomHub>,
        cert:     Arc<ServerCert>,
        admin_tx: broadcast::Sender<String>,
    ) -> std::io::Result<Self> {
        let addr = SocketAddr::from(([0, 0, 0, 0], config::UDP_PORT));
        let socket = UdpSocket::bind(addr).await?;
        info!("UDP transport bound on {}", addr);

        Ok(Self {
            socket: Arc::new(socket),
            room_hub,
            cert,
            dtls_map: DtlsSessionMap::new(),
            pkt_count: 0,
            dbg_rtp_count: AtomicU64::new(0),
            metrics: ServerMetrics::new(),
            admin_tx,
            spawn_rtp_relayed:  Arc::new(AtomicU64::new(0)),
            spawn_sr_relayed:   Arc::new(AtomicU64::new(0)),
            spawn_encrypt_fail: Arc::new(AtomicU64::new(0)),
        })
    }

    /// 외부에서 생성된 socket을 받아 구성 (AppState와 socket 공유 시)
    pub fn from_socket(
        socket:   Arc<UdpSocket>,
        room_hub: Arc<RoomHub>,
        cert:     Arc<ServerCert>,
        admin_tx: broadcast::Sender<String>,
    ) -> Self {
        Self {
            socket,
            room_hub,
            cert,
            dtls_map: DtlsSessionMap::new(),
            pkt_count: 0,
            dbg_rtp_count: AtomicU64::new(0),
            metrics: ServerMetrics::new(),
            admin_tx,
            spawn_rtp_relayed:  Arc::new(AtomicU64::new(0)),
            spawn_sr_relayed:   Arc::new(AtomicU64::new(0)),
            spawn_encrypt_fail: Arc::new(AtomicU64::new(0)),
        }
    }

    pub async fn run(mut self) {
        let mut buf = BytesMut::zeroed(config::UDP_RECV_BUF_SIZE);

        // 3초 주기 metrics flush 타이머
        let mut metrics_timer = tokio::time::interval(
            tokio::time::Duration::from_secs(3),
        );
        metrics_timer.tick().await; // 첨 tick 소비

        // REMB 전송 타이머 (1초 주기)
        let mut remb_timer = tokio::time::interval(
            tokio::time::Duration::from_millis(config::REMB_INTERVAL_MS),
        );
        remb_timer.tick().await; // 첨 tick 소비

        loop {
            tokio::select! {
                result = self.socket.recv_from(&mut buf) => {
                    let (len, remote) = match result {
                        Ok(r) => r,
                        Err(e) => { error!("UDP recv error: {e}"); continue; }
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
                _ = metrics_timer.tick() => {
                    self.flush_metrics();
                }
                _ = remb_timer.tick() => {
                    self.send_remb_to_publishers().await;
                }
            }
        }
    }

    /// 3초마다 metrics 집계 → admin_tx로 push → reset
    fn flush_metrics(&mut self) {
        let mut json = self.metrics.to_json();
        // Phase W-1: spawn fan-out atomic counters 포함
        let rtp_relayed = self.spawn_rtp_relayed.swap(0, Ordering::Relaxed);
        let sr_relayed  = self.spawn_sr_relayed.swap(0, Ordering::Relaxed);
        let enc_fail    = self.spawn_encrypt_fail.swap(0, Ordering::Relaxed);
        if let serde_json::Value::Object(ref mut map) = json {
            map.insert("spawn_rtp_relayed".into(), serde_json::json!(rtp_relayed));
            map.insert("spawn_sr_relayed".into(), serde_json::json!(sr_relayed));
            map.insert("spawn_encrypt_fail".into(), serde_json::json!(enc_fail));
        }
        let _ = self.admin_tx.send(json.to_string());
        self.metrics.reset();
    }

    // ========================================================================
    // REMB — 서버 자체 대역폭 힌트 (1초 주기)
    // ========================================================================

    /// 모든 room의 모든 publisher에게 REMB 전송
    /// Chrome BWE에게 "이 만큼까지 보내도 된다"는 힌트를 제공
    async fn send_remb_to_publishers(&mut self) {
        use crate::room::participant::TrackKind;

        for room_entry in self.room_hub.rooms.iter() {
            let room = room_entry.value();

            for entry in room.participants.iter() {
                let publisher = entry.value();
                if !publisher.is_publish_ready() { continue; }

                let pub_addr = match publisher.publish.get_address() {
                    Some(a) => a,
                    None => continue,
                };

                // video 트랙 SSRC 찾기
                let video_ssrc = {
                    let tracks = publisher.tracks.lock().unwrap();
                    tracks.iter()
                        .find(|t| t.kind == TrackKind::Video)
                        .map(|t| t.ssrc)
                };

                let ssrc = match video_ssrc {
                    Some(s) => s,
                    None => continue,
                };

                let remb_plain = build_remb(config::REMB_BITRATE_BPS, ssrc);

                // SRTCP 암호화 (publisher의 publish session outbound context)
                let encrypted = {
                    let mut ctx = publisher.publish.outbound_srtp.lock().unwrap();
                    match ctx.encrypt_rtcp(&remb_plain) {
                        Ok(p) => p,
                        Err(_) => continue,
                    }
                };

                if let Err(e) = self.socket.send_to(&encrypted, pub_addr).await {
                    debug!("[REMB] send FAILED user={} addr={}: {e}", publisher.user_id, pub_addr);
                }
            }
        }
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

                            // Subscribe SRTP ready → PLI to all publishers in room
                            if pc_type == PcType::Subscribe {
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

    // ========================================================================
    // SRTP — hot path (media relay)
    // ========================================================================

    async fn handle_srtp(&mut self, buf: &[u8], remote: SocketAddr) {
        let seq_num = self.dbg_rtp_count.fetch_add(1, Ordering::Relaxed);

        // O(1) lookup: addr → (participant, pc_type, room)
        let (sender, pc_type, room) = match self.room_hub.find_by_addr(&remote) {
            Some(r) => r,
            None => {
                if seq_num < config::DBG_DETAIL_LIMIT {
                    debug!("[DBG:RTP] from unknown addr={} srtp_len={}", remote, buf.len());
                }
                return;
            }
        };

        sender.touch(current_ts());

        // Subscribe PC에서 오는 패킷: RTCP feedback (NACK 등)
        if pc_type != PcType::Publish {
            self.handle_subscribe_rtcp(buf, remote, &sender, &room, seq_num).await;
            return;
        }

        if !sender.publish.is_media_ready() {
            if seq_num < config::DBG_DETAIL_LIMIT {
                debug!("[DBG:RTP] before DTLS complete user={} addr={}", sender.user_id, remote);
            }
            return;
        }

        // Detect RTCP vs RTP (RFC 5761 demux: PT 72-79 = RTCP)
        let is_rtcp = buf.get(1)
            .map(|b| { let pt = b & 0x7F; (72..=79).contains(&pt) })
            .unwrap_or(false);

        if is_rtcp {
            let is_detail = seq_num < config::DBG_DETAIL_LIMIT;

            // Decrypt SRTCP (publish session inbound)
            let plaintext = {
                let lock_t = Instant::now();
                let mut ctx = sender.publish.inbound_srtp.lock().unwrap();
                self.metrics.lock_wait.record(lock_t.elapsed().as_micros() as u64);
                let dec_t = Instant::now();
                match ctx.decrypt_rtcp(buf) {
                    Ok(p) => {
                        self.metrics.decrypt.record(dec_t.elapsed().as_micros() as u64);
                        p
                    }
                    Err(e) => {
                        self.metrics.decrypt_fail += 1;
                        if is_detail {
                            debug!("[DBG:RTCP:PUB] decrypt FAILED user={}: {e}", sender.user_id);
                        }
                        return;
                    }
                }
            };

            // Phase C-2a: SR relay — publisher의 SR을 모든 subscriber에게 전달
            self.relay_publish_rtcp(&plaintext, &sender, &room, is_detail).await;
            return;
        }

        // Decrypt SRTP → plaintext RTP (from publish session)
        let plaintext = {
            let lock_t = Instant::now();
            let mut ctx = sender.publish.inbound_srtp.lock().unwrap();
            self.metrics.lock_wait.record(lock_t.elapsed().as_micros() as u64);
            let dec_t = Instant::now();
            match ctx.decrypt_rtp(buf) {
                Ok(p) => {
                    self.metrics.decrypt.record(dec_t.elapsed().as_micros() as u64);
                    p
                }
                Err(e) => {
                    self.metrics.decrypt_fail += 1;
                    if seq_num < config::DBG_DETAIL_LIMIT {
                        debug!("[DBG:RTP] decrypt FAILED user={} addr={} srtp_len={}: {e}",
                            sender.user_id, remote, buf.len());
                    }
                    return;
                }
            }
        };

        // [DBG:RTP] Parse RTP header for logging
        let rtp_hdr = parse_rtp_header(&plaintext);
        let is_detail = seq_num < config::DBG_DETAIL_LIMIT;
        let is_summary = seq_num > 0 && seq_num % config::DBG_SUMMARY_INTERVAL == 0;

        // 비디오 RTP 캐시 (NACK → RTX 재전송용, audio는 skip)
        // PT 96 = VP8 (server_codec_policy)
        if rtp_hdr.pt == 96 {
            if let Ok(mut cache) = sender.rtp_cache.lock() {
                cache.store(rtp_hdr.seq, &plaintext);
            }
        }

        if is_detail {
            debug!("[DBG:RTP] #{} user={} ssrc=0x{:08X} pt={} seq={} ts={} marker={} payload_len={}",
                seq_num, sender.user_id,
                rtp_hdr.ssrc, rtp_hdr.pt, rtp_hdr.seq, rtp_hdr.timestamp,
                rtp_hdr.marker, plaintext.len().saturating_sub(rtp_hdr.header_len));
        } else if is_summary {
            trace!("[DBG:RTP] summary #{} user={} last_ssrc=0x{:08X} last_pt={} last_seq={}",
                seq_num, sender.user_id,
                rtp_hdr.ssrc, rtp_hdr.pt, rtp_hdr.seq);
        }

        // Phase W-1: fan-out을 tokio::spawn으로 분리
        // 메인 루프는 recv→decrypt→cache→spawn만 하고 즉시 다음 패킷 처리
        // spawn된 task는 Tokio work-stealing으로 유휴 코어에 분산
        let targets = room.other_participants(&sender.user_id);

        if is_detail {
            let target_info: Vec<String> = targets.iter()
                .filter(|t| t.is_subscribe_ready())
                .map(|t| format!("{}@{}", t.user_id, t.subscribe.get_address()
                    .map(|a| a.to_string()).unwrap_or("none".into())))
                .collect();
            debug!("[DBG:RELAY] #{} from={} targets=[{}]",
                seq_num, sender.user_id, target_info.join(", "));
        }

        let socket = Arc::clone(&self.socket);
        let relay_counter = Arc::clone(&self.spawn_rtp_relayed);
        let fail_counter = Arc::clone(&self.spawn_encrypt_fail);

        tokio::spawn(async move {
            let mut relay_count = 0u32;
            for target in &targets {
                if !target.is_subscribe_ready() { continue; }

                let addr = match target.subscribe.get_address() {
                    Some(a) => a,
                    None => continue,
                };

                let encrypted = {
                    let mut ctx = target.subscribe.outbound_srtp.lock().unwrap();
                    match ctx.encrypt_rtp(&plaintext) {
                        Ok(p) => p,
                        Err(_) => {
                            fail_counter.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                    }
                };

                if socket.send_to(&encrypted, addr).await.is_ok() {
                    relay_count += 1;
                }
            }
            relay_counter.fetch_add(relay_count as u64, Ordering::Relaxed);
        });
    }

    // ========================================================================
    // Subscribe RTCP — compound 파싱 → NACK 서버 처리 + 나머지 publisher 릴레이
    // ========================================================================

    /// Subscribe PC에서 수신된 RTCP 처리
    /// - NACK (PT=205): 서버에서 RTX 재전송 (기존 Phase C 로직)
    /// - RR/PLI/REMB: 해당 publisher의 publish PC로 transparent relay
    async fn handle_subscribe_rtcp(
        &mut self,
        buf: &[u8],
        remote: SocketAddr,
        subscriber: &Arc<crate::room::participant::Participant>,
        room: &Arc<crate::room::room::Room>,
        seq_num: u64,
    ) {
        let is_detail = seq_num < config::DBG_DETAIL_LIMIT;
        self.metrics.sub_rtcp_received += 1;

        // RTCP인지 확인 (RFC 5761: PT 72-79)
        let is_rtcp = buf.get(1)
            .map(|b| { let pt = b & 0x7F; (72..=79).contains(&pt) })
            .unwrap_or(false);

        if !is_rtcp {
            self.metrics.sub_rtcp_not_rtcp += 1;
            if is_detail {
                trace!("[DBG:SUB] non-RTCP from subscribe PC user={} addr={} byte0=0x{:02X} byte1=0x{:02X}",
                    subscriber.user_id, remote,
                    buf.get(0).copied().unwrap_or(0),
                    buf.get(1).copied().unwrap_or(0));
            }
            return;
        }

        // Subscribe session의 inbound_srtp로 decrypt
        let plaintext = {
            let mut ctx = subscriber.subscribe.inbound_srtp.lock().unwrap();
            match ctx.decrypt_rtcp(buf) {
                Ok(p) => {
                    self.metrics.sub_rtcp_decrypted += 1;
                    p
                }
                Err(e) => {
                    self.metrics.decrypt_fail += 1;
                    if is_detail {
                        debug!("[DBG:RTCP:SUB] SRTCP decrypt FAILED user={} addr={}: {e}",
                            subscriber.user_id, remote);
                    }
                    return;
                }
            }
        };

        // Compound RTCP 파싱: NACK 분리 + publisher별 릴레이 대상 수집
        let parsed = split_compound_rtcp(&plaintext);

        if is_detail {
            debug!("[DBG:RTCP:SUB] user={} compound_len={} nack_blocks={} relay_blocks={}",
                subscriber.user_id, plaintext.len(), parsed.nack_blocks.len(), parsed.relay_blocks.len());
        }

        // (1) NACK 처리 (RTX 재전송 — 기존 로직)
        self.metrics.nack_received += parsed.nack_blocks.len() as u64;
        for nack_block in &parsed.nack_blocks {
            self.handle_nack_block(nack_block, subscriber, room, is_detail).await;
        }

        // (2) 릴레이 대상 RTCP (RR, PLI, REMB) → publisher별로 모아서 전송
        if parsed.relay_blocks.is_empty() {
            return;
        }

        // RR/PLI relay count — plaintext는 이미 복호화된 RTCP이므로 & 0x7F 마스크 불필요
        for block in &parsed.relay_blocks {
            let pt = plaintext.get(block.offset + 1).copied().unwrap_or(0);
            if pt == config::RTCP_PT_RR { self.metrics.rr_relayed += 1; }
            if pt == config::RTCP_PT_PSFB { self.metrics.pli_sent += 1; }
        }

        // media_ssrc → publisher 매핑 + RTCP 블록 그룹핑
        let mut publisher_rtcp: HashMap<u32, Vec<&[u8]>> = HashMap::new();
        for block in &parsed.relay_blocks {
            if block.media_ssrc == 0 { continue; } // RC=0 RR 등 media_ssrc 없는 경우 skip
            publisher_rtcp.entry(block.media_ssrc)
                .or_default()
                .push(&plaintext[block.offset..block.offset + block.length]);
        }

        for (media_ssrc, blocks) in &publisher_rtcp {
            // publisher 찾기
            let publisher = room.all_participants().into_iter()
                .find(|p| p.get_tracks().iter().any(|t| t.ssrc == *media_ssrc));

            let publisher = match publisher {
                Some(p) => p,
                None => {
                    if is_detail {
                        debug!("[DBG:RTCP:SUB] publisher not found for ssrc=0x{:08X}", media_ssrc);
                    }
                    continue;
                }
            };

            if !publisher.is_publish_ready() { continue; }

            let pub_addr = match publisher.publish.get_address() {
                Some(a) => a,
                None => continue,
            };

            // 릴레이 대상 RTCP 블록들을 compound로 재조립
            let compound = assemble_compound(blocks);

            // publisher의 publish session outbound_srtp로 encrypt
            let encrypted = {
                let mut ctx = publisher.publish.outbound_srtp.lock().unwrap();
                match ctx.encrypt_rtcp(&compound) {
                    Ok(p) => p,
                    Err(e) => {
                        if is_detail {
                            debug!("[DBG:RTCP:SUB] relay encrypt FAILED ssrc=0x{:08X}: {e}", media_ssrc);
                        }
                        continue;
                    }
                }
            };

            if let Err(e) = self.socket.send_to(&encrypted, pub_addr).await {
                if is_detail {
                    debug!("[DBG:RTCP:SUB] relay send FAILED ssrc=0x{:08X} addr={}: {e}",
                        media_ssrc, pub_addr);
                }
            } else {
                if is_detail {
                    debug!("[DBG:RTCP:SUB] relayed {} block(s) ssrc=0x{:08X} → user={} addr={}",
                        blocks.len(), media_ssrc, publisher.user_id, pub_addr);
                }
            }
        }
    }

    // ========================================================================
    // NACK 처리 (RTX 재전송 — 기존 Phase C 로직 추출)
    // ========================================================================

    /// 단일 NACK RTCP 블록 처리: 캐시 조회 → RTX 조립 → 전송
    async fn handle_nack_block(
        &mut self,
        nack_data: &[u8],
        subscriber: &Arc<crate::room::participant::Participant>,
        room: &Arc<crate::room::room::Room>,
        is_detail: bool,
    ) {
        let nack_items = parse_rtcp_nack(nack_data);

        for nack in &nack_items {
            let lost_seqs = expand_nack(nack.pid, nack.blp);

            if is_detail {
                debug!("[DBG:NACK] user={} media_ssrc=0x{:08X} pid={} blp=0x{:04X} seqs={:?}",
                    subscriber.user_id, nack.media_ssrc, nack.pid, nack.blp, lost_seqs);
            }

            // 해당 media_ssrc의 publisher 찾기
            let publisher = room.all_participants().into_iter()
                .find(|p| p.get_tracks().iter().any(|t| t.ssrc == nack.media_ssrc));

            let publisher = match publisher {
                Some(p) => p,
                None => {
                    if is_detail {
                        debug!("[DBG:NACK] publisher not found for ssrc=0x{:08X}", nack.media_ssrc);
                    }
                    continue;
                }
            };

            // RTX SSRC 찾기
            let rtx_ssrc = publisher.get_tracks().iter()
                .find(|t| t.ssrc == nack.media_ssrc)
                .and_then(|t| t.rtx_ssrc);

            let rtx_ssrc = match rtx_ssrc {
                Some(s) => s,
                None => {
                    if is_detail {
                        debug!("[DBG:NACK] no rtx_ssrc for ssrc=0x{:08X}", nack.media_ssrc);
                    }
                    continue;
                }
            };

            let sub_addr = match subscriber.subscribe.get_address() {
                Some(a) => a,
                None => continue,
            };

            // 캐시 조회 + RTX 조립
            let rtx_packets: Vec<(u16, u16, Vec<u8>)> = {
                let cache = publisher.rtp_cache.lock().unwrap();
                lost_seqs.iter().filter_map(|&lost_seq| {
                    let original = cache.get(lost_seq)?;
                    let rtx_seq = publisher.next_rtx_seq();
                    let rtx_pkt = build_rtx_packet(original, rtx_ssrc, rtx_seq);
                    Some((lost_seq, rtx_seq, rtx_pkt))
                }).collect()
            };

            let cache_miss = lost_seqs.len() - rtx_packets.len();
            self.metrics.rtx_cache_miss += cache_miss as u64;
            self.metrics.rtx_sent += rtx_packets.len() as u64;

            if is_detail && cache_miss > 0 {
                trace!("[DBG:RTX] cache miss {}/{} seqs for ssrc=0x{:08X}",
                    cache_miss, lost_seqs.len(), nack.media_ssrc);
            }

            for (lost_seq, rtx_seq, rtx_pkt) in &rtx_packets {
                let encrypted = {
                    let mut ctx = subscriber.subscribe.outbound_srtp.lock().unwrap();
                    match ctx.encrypt_rtp(rtx_pkt) {
                        Ok(p) => p,
                        Err(e) => {
                            if is_detail {
                                debug!("[DBG:RTX] encrypt FAILED seq={}: {e}", lost_seq);
                            }
                            continue;
                        }
                    }
                };

                if let Err(e) = self.socket.send_to(&encrypted, sub_addr).await {
                    if is_detail {
                        debug!("[DBG:RTX] send FAILED seq={} addr={}: {e}", lost_seq, sub_addr);
                    }
                } else if is_detail {
                    debug!("[DBG:RTX] sent seq={} rtx_seq={} rtx_ssrc=0x{:08X} → user={} addr={}",
                        lost_seq, rtx_seq, rtx_ssrc, subscriber.user_id, sub_addr);
                }
            }
        }
    }

    // ========================================================================
    // Publish RTCP relay — SR을 모든 subscriber에게 fan-out (Phase C-2a)
    // ========================================================================

    /// Publish PC에서 수신된 RTCP compound를 모든 subscriber에게 릴레이.
    /// SR 외 다른 RTCP도 함께 있을 수 있으나, compound 통째로 릴레이한다.
    /// (publish → subscribe 방향에는 NACK이 없으므로 분리 불필요)
    async fn relay_publish_rtcp(
        &mut self,
        plaintext: &[u8],
        sender: &Arc<crate::room::participant::Participant>,
        room: &Arc<crate::room::room::Room>,
        _is_detail: bool,
    ) {
        self.metrics.sr_relayed += 1;

        // Phase W-1: SR fan-out도 tokio::spawn으로 분리
        let targets = room.other_participants(&sender.user_id);
        let plaintext_owned = plaintext.to_vec();
        let socket = Arc::clone(&self.socket);
        let relay_counter = Arc::clone(&self.spawn_sr_relayed);
        let fail_counter = Arc::clone(&self.spawn_encrypt_fail);

        tokio::spawn(async move {
            let mut relay_count = 0u32;
            for target in &targets {
                if !target.is_subscribe_ready() { continue; }

                let addr = match target.subscribe.get_address() {
                    Some(a) => a,
                    None => continue,
                };

                let encrypted = {
                    let mut ctx = target.subscribe.outbound_srtp.lock().unwrap();
                    match ctx.encrypt_rtcp(&plaintext_owned) {
                        Ok(p) => p,
                        Err(_) => {
                            fail_counter.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                    }
                };

                if socket.send_to(&encrypted, addr).await.is_ok() {
                    relay_count += 1;
                }
            }
            relay_counter.fetch_add(relay_count as u64, Ordering::Relaxed);
        });
    }
}

// ============================================================================
// NACK 파싱 (RFC 4585 Generic NACK)
// ============================================================================

/// NACK FCI (Feedback Control Information) 항목
struct NackItem {
    media_ssrc: u32,
    pid: u16,
    blp: u16,
}

/// RTCP compound 패킷에서 Generic NACK (PT=205, FMT=1) 항목 추출
///
/// RTCP 패킷 구조 (RFC 4585):
///   Byte 0: V=2, P, FMT (5bit)
///   Byte 1: PT (8bit)
///   Bytes 2-3: length (32-bit words - 1)
///   Bytes 4-7: SSRC of sender
///   Bytes 8-11: SSRC of media source
///   Bytes 12+: FCI (pid:16 + blp:16) × N
fn parse_rtcp_nack(buf: &[u8]) -> Vec<NackItem> {
    let mut items = Vec::new();
    let mut offset = 0;

    while offset + 4 <= buf.len() {
        if buf.len() < offset + 4 { break; }

        let fmt = buf[offset] & 0x1F;
        let pt  = buf[offset + 1];
        let length_words = u16::from_be_bytes([buf[offset + 2], buf[offset + 3]]) as usize;
        let pkt_len = (length_words + 1) * 4;

        if pt == config::RTCP_PT_NACK && fmt == config::RTCP_FMT_NACK {
            // Generic NACK
            if offset + 12 <= buf.len() {
                let media_ssrc = u32::from_be_bytes([
                    buf[offset + 8], buf[offset + 9],
                    buf[offset + 10], buf[offset + 11],
                ]);

                // FCI: (pid:16 + blp:16) × N
                let fci_start = offset + 12;
                let fci_end = (offset + pkt_len).min(buf.len());
                let mut fci_off = fci_start;
                while fci_off + 4 <= fci_end {
                    let pid = u16::from_be_bytes([buf[fci_off], buf[fci_off + 1]]);
                    let blp = u16::from_be_bytes([buf[fci_off + 2], buf[fci_off + 3]]);
                    items.push(NackItem { media_ssrc, pid, blp });
                    fci_off += 4;
                }
            }
        }

        offset += pkt_len;
        if pkt_len == 0 { break; } // safety
    }

    items
}

// ============================================================================
// Compound RTCP 파싱 + 분리 (Phase C-2)
// ============================================================================

/// Compound RTCP 파싱 결과
struct CompoundRtcpParsed {
    /// NACK 블록 (PT=205): 서버에서 RTX 처리
    nack_blocks: Vec<Vec<u8>>,
    /// 릴레이 대상 블록 (RR, PLI, REMB 등): publisher로 전달
    relay_blocks: Vec<RtcpBlockRef>,
}

/// Compound 내 개별 RTCP 블록 참조 (offset + length + media_ssrc)
struct RtcpBlockRef {
    offset: usize,
    length: usize,
    /// RTCP 내 media source SSRC (RR/PLI/REMB: bytes 8-11)
    media_ssrc: u32,
}

/// Compound RTCP를 순회하며 NACK / relay 대상으로 분류
///
/// - PT=205 FMT=1 (Generic NACK): nack_blocks로 분리 (서버 처리)
/// - PT=201 (RR), PT=206 FMT=1 (PLI), PT=206 FMT=15 (REMB): relay_blocks로 수집
/// - 그 외: 무시
fn split_compound_rtcp(buf: &[u8]) -> CompoundRtcpParsed {
    let mut result = CompoundRtcpParsed {
        nack_blocks: Vec::new(),
        relay_blocks: Vec::new(),
    };

    let mut offset = 0;
    while offset + 4 <= buf.len() {
        let fmt = buf[offset] & 0x1F;
        let pt  = buf[offset + 1];
        let length_words = u16::from_be_bytes([buf[offset + 2], buf[offset + 3]]) as usize;
        let pkt_len = (length_words + 1) * 4;

        if pkt_len == 0 || offset + pkt_len > buf.len() { break; }

        // media_ssrc: 대부분 RTCP에서 bytes 8-11 (패킷 내 2번째 SSRC)
        let media_ssrc = if offset + 12 <= buf.len() {
            u32::from_be_bytes([
                buf[offset + 8], buf[offset + 9],
                buf[offset + 10], buf[offset + 11],
            ])
        } else {
            0
        };

        if pt == config::RTCP_PT_NACK && fmt == config::RTCP_FMT_NACK {
            // NACK: 별도 복사하여 nack_blocks에 저장
            result.nack_blocks.push(buf[offset..offset + pkt_len].to_vec());
        } else if pt == config::RTCP_PT_RR
            || (pt == config::RTCP_PT_PSFB && fmt == config::RTCP_FMT_PLI)
            || (pt == config::RTCP_PT_PSFB && fmt == config::RTCP_FMT_REMB)
        {
            // 릴레이 대상: RR, PLI, REMB
            result.relay_blocks.push(RtcpBlockRef {
                offset,
                length: pkt_len,
                media_ssrc,
            });
        }
        // 그 외 (SR 등 subscribe에서 오는 것): 무시

        offset += pkt_len;
    }

    result
}

/// 릴레이 대상 RTCP 블록들을 하나의 compound 패킷으로 재조립
fn assemble_compound(blocks: &[&[u8]]) -> Vec<u8> {
    let total: usize = blocks.iter().map(|b| b.len()).sum();
    let mut buf = Vec::with_capacity(total);
    for block in blocks {
        buf.extend_from_slice(block);
    }
    buf
}

/// NACK PID + BLP → 손실 seq 목록 확장
///
/// PID = 첫 번째 손실 seq
/// BLP = 비트마스크, bit i 설정 → (PID + i + 1) 손실
fn expand_nack(pid: u16, blp: u16) -> Vec<u16> {
    let mut seqs = vec![pid];
    for i in 0..16u16 {
        if blp & (1 << i) != 0 {
            seqs.push(pid.wrapping_add(i + 1));
        }
    }
    seqs
}

// ============================================================================
// RTX 패킷 조립 (RFC 4588)
// ============================================================================

/// 원본 RTP plaintext → RTX 패킷 조립
///
/// RTX 패킷 구조:
///   - RTP 헤더: V/P/X/CC 동일, M=0, PT=97(RTX), seq=rtx_seq, ts=원본, SSRC=rtx_ssrc
///   - 페이로드: [원본 seq 2바이트 big-endian] + [원본 RTP 페이로드]
fn build_rtx_packet(original: &[u8], rtx_ssrc: u32, rtx_seq: u16) -> Vec<u8> {
    if original.len() < config::RTP_HEADER_MIN_SIZE {
        return Vec::new();
    }

    let cc = (original[0] & 0x0F) as usize;
    let header_len = 12 + cc * 4;
    if original.len() < header_len {
        return Vec::new();
    }

    let orig_seq = u16::from_be_bytes([original[2], original[3]]);
    let payload = &original[header_len..];

    // RTX 패킷 = header(12+cc*4) + orig_seq(2) + payload
    let mut rtx = Vec::with_capacity(header_len + 2 + payload.len());

    // RTP header 복사 (V/P/X/CC 유지)
    rtx.extend_from_slice(&original[..header_len]);

    // PT → RTX (97), M=0
    rtx[1] = config::RTX_PAYLOAD_TYPE;

    // seq → rtx_seq
    rtx[2..4].copy_from_slice(&rtx_seq.to_be_bytes());

    // SSRC → rtx_ssrc
    rtx[8..12].copy_from_slice(&rtx_ssrc.to_be_bytes());

    // OSN (Original Sequence Number) + 원본 페이로드
    rtx.extend_from_slice(&orig_seq.to_be_bytes());
    rtx.extend_from_slice(payload);

    rtx
}

// ============================================================================
// Utility
// ============================================================================

fn current_ts() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

struct RtpHeader {
    pt:         u8,
    marker:     bool,
    seq:        u16,
    timestamp:  u32,
    ssrc:       u32,
    header_len: usize,
}

fn parse_rtp_header(buf: &[u8]) -> RtpHeader {
    if buf.len() < config::RTP_HEADER_MIN_SIZE {
        return RtpHeader { pt: 0, marker: false, seq: 0, timestamp: 0, ssrc: 0, header_len: 0 };
    }
    let b1 = buf[1];
    let cc = (buf[0] & 0x0F) as usize;
    RtpHeader {
        pt:         b1 & 0x7F,
        marker:     (b1 & 0x80) != 0,
        seq:        u16::from_be_bytes([buf[2], buf[3]]),
        timestamp:  u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]),
        ssrc:       u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]),
        header_len: 12 + cc * 4,
    }
}

// ============================================================================
// RTCP PLI builder (RFC 4585, 12 bytes fixed)
// ============================================================================
//
//  0               1               2               3
//  0 1 2 3 4 5 6 7 0 1 2 3 4 5 6 7 0 1 2 3 4 5 6 7 0 1 2 3 4 5 6 7
// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
// |V=2|P| FMT=1  |   PT=206      |          length=2             |
// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
// |                  SSRC of packet sender (0)                   |
// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
// |                  SSRC of media source                        |
// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+

pub fn build_pli(media_ssrc: u32) -> [u8; 12] {
    let mut buf = [0u8; 12];
    // V=2, P=0, FMT=1 → 0b10_0_00001 = 0x81
    buf[0] = 0x81;
    // PT=206 (PSFB)
    buf[1] = 206;
    // length=2 (in 32-bit words minus 1)
    buf[2] = 0;
    buf[3] = 2;
    // SSRC of sender = 0 (server doesn't have its own SSRC)
    // buf[4..8] already 0
    // SSRC of media source
    buf[8..12].copy_from_slice(&media_ssrc.to_be_bytes());
    buf
}

// ============================================================================
// RTCP REMB builder (draft-alvestrand-rmcat-remb)
// ============================================================================
//
//  0               1               2               3
//  0 1 2 3 4 5 6 7 0 1 2 3 4 5 6 7 0 1 2 3 4 5 6 7 0 1 2 3 4 5 6 7
// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
// |V=2|P| FMT=15 |   PT=206      |          length=5             |
// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
// |                  SSRC of packet sender (0)                   |
// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
// |                  SSRC of media source (0)                    |
// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
// |  'R' 'E' 'M' 'B'                                            |
// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
// | Num SSRC=1  | BR Exp  |  BR Mantissa (18 bits)              |
// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
// |   SSRC feedback (video SSRC)                                 |
// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+

/// 서버 자체 REMB 패킷 생성 (24바이트 고정, SSRC 1개)
///
/// Chrome의 goog-remb rtcp_fb에 대응. 서버가 publisher에게
/// "이 만큼까지 보내도 된다"는 대역폭 힌트를 제공한다.
fn build_remb(bitrate_bps: u64, media_ssrc: u32) -> [u8; 24] {
    let mut buf = [0u8; 24];

    // V=2, P=0, FMT=15 → 0b10_0_01111 = 0x8F
    buf[0] = 0x8F;
    // PT=206 (PSFB)
    buf[1] = 206;
    // length=5 (24 bytes / 4 - 1)
    buf[2] = 0;
    buf[3] = 5;
    // SSRC of sender = 0
    // buf[4..8] already 0
    // SSRC of media source = 0
    // buf[8..12] already 0

    // 'R' 'E' 'M' 'B'
    buf[12] = b'R';
    buf[13] = b'E';
    buf[14] = b'M';
    buf[15] = b'B';

    // Num SSRC = 1, BR Exp (6 bits), BR Mantissa (18 bits)
    // bitrate_bps = mantissa * 2^exp
    let (exp, mantissa) = encode_remb_bitrate(bitrate_bps);

    // Byte 16: Num SSRC (1)
    // Byte 16-19: [num_ssrc:8][exp:6][mantissa:18]
    buf[16] = 1; // num SSRC
    buf[17] = (exp << 2) | ((mantissa >> 16) as u8 & 0x03);
    buf[18] = (mantissa >> 8) as u8;
    buf[19] = mantissa as u8;

    // SSRC feedback
    buf[20..24].copy_from_slice(&media_ssrc.to_be_bytes());

    buf
}

/// REMB 비트레이트 인코딩: bps → (exp, mantissa)
/// mantissa * 2^exp = bps, mantissa는 18비트 이하
fn encode_remb_bitrate(bps: u64) -> (u8, u32) {
    let mut exp: u8 = 0;
    let mut mantissa = bps;
    while mantissa > 0x3FFFF { // 18 bits max = 262143
        mantissa >>= 1;
        exp += 1;
    }
    (exp, mantissa as u32)
}

/// Subscribe SRTP ready 시 해당 room의 모든 publisher에게 PLI 전송
async fn send_pli_to_publishers(
    socket: &UdpSocket,
    room: &crate::room::room::Room,
    exclude_user: &str,
) {
    use crate::room::participant::TrackKind;

    for entry in room.participants.iter() {
        let publisher = entry.value();
        if publisher.user_id == exclude_user { continue; }
        if !publisher.is_publish_ready() { continue; }

        let pub_addr = match publisher.publish.get_address() {
            Some(a) => a,
            None => continue,
        };

        // publisher의 video 트랙 SSRC 찾기
        let video_ssrc = {
            let tracks = publisher.tracks.lock().unwrap();
            tracks.iter()
                .find(|t| t.kind == TrackKind::Video)
                .map(|t| t.ssrc)
        };

        let ssrc = match video_ssrc {
            Some(s) => s,
            None => continue, // video 트랙 없으면 skip
        };

        let pli_plain = build_pli(ssrc);

        // SRTCP 암호화 (publisher의 publish session outbound context)
        let encrypted = {
            let mut ctx = publisher.publish.outbound_srtp.lock().unwrap();
            match ctx.encrypt_rtcp(&pli_plain) {
                Ok(p) => p,
                Err(e) => {
                    warn!("[DBG:PLI] encrypt FAILED → user={}: {e}", publisher.user_id);
                    continue;
                }
            }
        };

        if let Err(e) = socket.send_to(&encrypted, pub_addr).await {
            warn!("[DBG:PLI] send FAILED → user={} addr={}: {e}", publisher.user_id, pub_addr);
        } else {
            info!("[DBG:PLI] sent → user={} ssrc=0x{:08X} addr={}",
                publisher.user_id, ssrc, pub_addr);
        }
    }
}
