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
//!              PcType::Subscribe → RTCP feedback (future)

use bytes::{Bytes, BytesMut};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tracing::{debug, error, info, trace, warn};

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
}

impl UdpTransport {
    pub async fn bind(
        room_hub: Arc<RoomHub>,
        cert:     Arc<ServerCert>,
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
        })
    }

    pub async fn run(mut self) {
        let mut buf = BytesMut::zeroed(config::UDP_RECV_BUF_SIZE);

        loop {
            let (len, remote) = match self.socket.recv_from(&mut buf).await {
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

            debug!("[DBG:DTLS] session ended user={} pc={} addr={}",
                participant.user_id, pc_type, remote);
        });
    }

    // ========================================================================
    // SRTP — hot path (media relay)
    // ========================================================================

    async fn handle_srtp(&self, buf: &[u8], remote: SocketAddr) {
        let seq_num = self.dbg_rtp_count.fetch_add(1, Ordering::Relaxed);

        // O(1) lookup: addr → (participant, pc_type, room)
        let (sender, pc_type, room) = match self.room_hub.find_by_addr(&remote) {
            Some(r) => r,
            None => {
                if seq_num < config::DBG_DETAIL_LIMIT {
                    info!("[DBG:RTP] from unknown addr={} srtp_len={}", remote, buf.len());
                }
                return;
            }
        };

        sender.touch(current_ts());

        // Only process media from publish PC
        if pc_type != PcType::Publish {
            // Subscribe PC에서 오는 SRTP는 RTCP feedback (future Phase)
            if seq_num < config::DBG_DETAIL_LIMIT {
                trace!("[DBG:RTP] from subscribe PC user={} addr={} (RTCP feedback — ignored)",
                    sender.user_id, remote);
            }
            return;
        }

        if !sender.publish.is_media_ready() {
            if seq_num < config::DBG_DETAIL_LIMIT {
                info!("[DBG:RTP] before DTLS complete user={} addr={}", sender.user_id, remote);
            }
            return;
        }

        // Detect RTCP vs RTP (RFC 5761 demux: PT 72-79 = RTCP)
        let is_rtcp = buf.get(1)
            .map(|b| { let pt = b & 0x7F; (72..=79).contains(&pt) })
            .unwrap_or(false);

        if is_rtcp {
            let rtcp_pt = buf.get(1).map(|b| b & 0x7F).unwrap_or(0);
            let rtcp_ssrc = parse_ssrc(buf);
            let mut ctx = sender.publish.inbound_srtp.lock().unwrap();
            match ctx.decrypt_rtcp(buf) {
                Ok(plain) => {
                    if seq_num < config::DBG_DETAIL_LIMIT {
                        info!("[DBG:RTCP] user={} pt={} ssrc=0x{:08X} srtp_len={} plain_len={}",
                            sender.user_id, rtcp_pt, rtcp_ssrc, buf.len(), plain.len());
                    }
                }
                Err(e) => {
                    if seq_num < config::DBG_DETAIL_LIMIT {
                        info!("[DBG:RTCP] decrypt FAILED user={} pt={} ssrc=0x{:08X}: {e}",
                            sender.user_id, rtcp_pt, rtcp_ssrc);
                    }
                }
            }
            return; // RTCP relay — Phase C
        }

        // Decrypt SRTP → plaintext RTP (from publish session)
        let plaintext = {
            let mut ctx = sender.publish.inbound_srtp.lock().unwrap();
            match ctx.decrypt_rtp(buf) {
                Ok(p) => p,
                Err(e) => {
                    if seq_num < config::DBG_DETAIL_LIMIT {
                        info!("[DBG:RTP] decrypt FAILED user={} addr={} srtp_len={}: {e}",
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

        if is_detail {
            info!("[DBG:RTP] #{} user={} ssrc=0x{:08X} pt={} seq={} ts={} marker={} payload_len={}",
                seq_num, sender.user_id,
                rtp_hdr.ssrc, rtp_hdr.pt, rtp_hdr.seq, rtp_hdr.timestamp,
                rtp_hdr.marker, plaintext.len().saturating_sub(rtp_hdr.header_len));
        } else if is_summary {
            info!("[DBG:RTP] summary #{} user={} last_ssrc=0x{:08X} last_pt={} last_seq={}",
                seq_num, sender.user_id,
                rtp_hdr.ssrc, rtp_hdr.pt, rtp_hdr.seq);
        }

        // Fan-out: relay to all other participants via their SUBSCRIBE session
        let targets = room.other_participants(&sender.user_id);

        if is_detail {
            let target_info: Vec<String> = targets.iter()
                .filter(|t| t.is_subscribe_ready())
                .map(|t| format!("{}@{}", t.user_id, t.subscribe.get_address()
                    .map(|a| a.to_string()).unwrap_or("none".into())))
                .collect();
            info!("[DBG:RELAY] #{} from={} targets=[{}]",
                seq_num, sender.user_id, target_info.join(", "));
        }

        let mut relay_count = 0u32;
        for target in &targets {
            // Use subscribe session for sending media TO the target
            if !target.is_subscribe_ready() { continue; }

            let addr = match target.subscribe.get_address() {
                Some(a) => a,
                None => continue,
            };

            let encrypted = {
                let mut ctx = target.subscribe.outbound_srtp.lock().unwrap();
                match ctx.encrypt_rtp(&plaintext) {
                    Ok(p) => p,
                    Err(e) => {
                        if is_detail {
                            info!("[DBG:RELAY] encrypt FAILED → user={}: {e}", target.user_id);
                        }
                        continue;
                    }
                }
            };

            if let Err(e) = self.socket.send_to(&encrypted, addr).await {
                if is_detail {
                    info!("[DBG:RELAY] send FAILED → user={} addr={}: {e}", target.user_id, addr);
                }
            } else {
                relay_count += 1;
                if is_detail {
                    info!("[DBG:RELAY] #{} → user={} addr={} enc_len={}",
                        seq_num, target.user_id, addr, encrypted.len());
                }
            }
        }

        if is_summary {
            info!("[DBG:RELAY] summary #{} from={} ssrc=0x{:08X} relayed_to={}",
                seq_num, sender.user_id, rtp_hdr.ssrc, relay_count);
        }
    }
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

fn parse_ssrc(buf: &[u8]) -> u32 {
    if buf.len() < 8 { return 0; }
    u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]])
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

fn build_pli(media_ssrc: u32) -> [u8; 12] {
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
