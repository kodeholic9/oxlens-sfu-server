// author: kodeholic (powered by Claude)
//! UDP media transport — single port, demux dispatch, RoomHub integration
//!
//! Packet flow:
//!   recv_from(addr)
//!     → classify (RFC 5764 first-byte)
//!     → STUN : RoomHub.latch_by_ufrag() → Binding Response → trigger DTLS
//!     → DTLS : DtlsSessionMap.inject() or start new handshake
//!     → SRTP : RoomHub.find_by_addr() → decrypt → relay → encrypt → send

use bytes::{Bytes, BytesMut};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tracing::{debug, error, info, trace, warn};

use webrtc_util::conn::Conn;

use std::sync::atomic::{AtomicU64, Ordering};

use crate::config;
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

    /// Inject packet into existing session. Returns false if no session.
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

    /// Periodically clean up sessions whose tx channel is closed
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
    /// Counter for periodic stale session cleanup
    pkt_count:    u64,
    /// [DBG] RTP hot-path packet counter (for log throttling)
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

    /// Main receive loop — runs forever
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

            // Periodic cleanup (every ~1000 packets)
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
        // Parse STUN message
        let msg = match stun::parse(buf) {
            Some(m) => m,
            None => { trace!("STUN parse failed from {}", remote); return; }
        };

        // Only handle Binding Requests
        if msg.msg_type != stun::BINDING_REQUEST {
            trace!("non-binding STUN from {} type=0x{:04X}", remote, msg.msg_type);
            return;
        }

        // USERNAME = "server_ufrag:client_ufrag"
        let username = match msg.username() {
            Some(u) => u,
            None => { debug!("STUN without USERNAME from {}", remote); return; }
        };
        let server_ufrag = match username.split(':').next() {
            Some(s) => s,
            None => { debug!("invalid STUN USERNAME format: {}", username); return; }
        };

        // Latch via RoomHub → registers addr reverse index
        let (participant, _room) = match self.room_hub.latch_by_ufrag(server_ufrag, remote) {
            Some(r) => r,
            None => { debug!("unknown ufrag={} from {}", server_ufrag, remote); return; }
        };

        participant.touch(current_ts());

        // [DBG:STUN] latch 성공
        info!("[DBG:STUN] latch user={} ufrag={} addr={}",
            participant.user_id, server_ufrag, remote);

        // Verify MESSAGE-INTEGRITY with participant's ice_pwd
        let integrity_key = stun::ice_integrity_key(&participant.ice_pwd);
        if !stun::verify_message_integrity(&msg, &integrity_key) {
            warn!("[DBG:STUN] MESSAGE-INTEGRITY mismatch user={} addr={}",
                participant.user_id, remote);
            return;
        }

        // Build and send Binding Success Response
        let response = stun::build_binding_response(
            &msg.transaction_id,
            remote,
            &integrity_key,
        );
        if let Err(e) = self.socket.send_to(&response, remote).await {
            error!("STUN response send failed: {e}");
        }
        info!("[DBG:STUN] binding-response sent user={} addr={} len={}",
            participant.user_id, remote, response.len());

        // USE-CANDIDATE → trigger DTLS handshake (if not already running)
        if msg.has_use_candidate() && !self.dtls_map.has(&remote) {
            info!("[DBG:STUN] USE-CANDIDATE user={} addr={} → starting DTLS",
                participant.user_id, remote);
            self.start_dtls_handshake(remote, participant).await;
        }
    }

    // ========================================================================
    // DTLS — handshake path
    // ========================================================================

    async fn handle_dtls(&mut self, data: Bytes, remote: SocketAddr) {
        // Try injecting into existing session first
        if self.dtls_map.inject(&remote, data.clone()).await {
            return;
        }

        // No session yet — check if participant is latched
        let (participant, _room) = match self.room_hub.find_by_addr(&remote) {
            Some(r) => r,
            None => {
                info!("[DBG:DTLS] from unlatched addr={}, dropping", remote);
                return;
            }
        };

        // Start new session + inject the first packet
        info!("[DBG:DTLS] new session user={} addr={} pkt_len={}",
            participant.user_id, remote, data.len());
        self.start_dtls_handshake(remote, participant).await;
        self.dtls_map.inject(&remote, data).await;
    }

    async fn start_dtls_handshake(
        &mut self,
        remote: SocketAddr,
        participant: Arc<crate::room::participant::Participant>,
    ) {
        let (adapter, tx) = DemuxConn::new(Arc::clone(&self.socket), remote);
        self.dtls_map.insert(remote, tx);

        let cert = Arc::clone(&self.cert);

        tokio::spawn(async move {
            info!("[DBG:DTLS] handshake starting user={} addr={}", participant.user_id, remote);
            let config = dtls::server_config(&cert);
            let conn: Arc<dyn webrtc_util::conn::Conn + Send + Sync> = Arc::new(adapter);

            let timeout = tokio::time::Duration::from_secs(10);
            let result = tokio::time::timeout(timeout, dtls::accept_dtls(conn, config)).await;

            match result {
                Ok(Ok(dtls_conn)) => {
                    info!("[DBG:DTLS] handshake OK user={} addr={}", participant.user_id, remote);
                    match dtls::export_srtp_keys(&dtls_conn).await {
                        Ok(keys) => {
                            info!("[DBG:DTLS] SRTP keys exported user={} client_key_len={} server_key_len={}",
                                participant.user_id, keys.client_key.len(), keys.server_key.len());
                            participant.install_srtp_keys(
                                &keys.client_key,
                                &keys.client_salt,
                                &keys.server_key,
                                &keys.server_salt,
                            );
                            info!("[DBG:DTLS] SRTP ready user={} addr={} media_ready={}",
                                participant.user_id, remote, participant.is_media_ready());
                        }
                        Err(e) => {
                            error!("[DBG:DTLS] SRTP key export FAILED user={}: {e}", participant.user_id);
                        }
                    }

                    // Keep DTLSConn alive — recv loop until connection ends
                    let mut keepalive_buf = vec![0u8; 1500];
                    loop {
                        match dtls_conn.recv(&mut keepalive_buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {} // application data (unused in SFU)
                        }
                    }
                }
                Ok(Err(e)) => {
                    warn!("[DBG:DTLS] handshake FAILED user={}: {e}", participant.user_id);
                }
                Err(_) => {
                    warn!("[DBG:DTLS] handshake TIMEOUT (10s) user={} addr={}", participant.user_id, remote);
                }
            }

            debug!("[DBG:DTLS] session ended user={} addr={}", participant.user_id, remote);
            // tx drops here → remove_stale() will clean up
        });
    }

    // ========================================================================
    // SRTP — hot path (media relay)
    // ========================================================================

    async fn handle_srtp(&self, buf: &[u8], remote: SocketAddr) {
        let seq_num = self.dbg_rtp_count.fetch_add(1, Ordering::Relaxed);

        // O(1) lookup: addr → participant + room
        let (sender, room) = match self.room_hub.find_by_addr(&remote) {
            Some(r) => r,
            None => {
                if seq_num < config::DBG_DETAIL_LIMIT {
                    info!("[DBG:RTP] from unknown addr={} srtp_len={}", remote, buf.len());
                }
                return;
            }
        };

        sender.touch(current_ts());

        if !sender.is_media_ready() {
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
            let mut ctx = sender.inbound_srtp.lock().unwrap();
            match ctx.decrypt_rtcp(buf) {
                Ok(plain) => {
                    if seq_num < config::DBG_DETAIL_LIMIT {
                        info!("[DBG:RTCP] user={} pt={} ssrc=0x{:08X} srtp_len={} plain_len={}",
                            sender.user_id, rtcp_pt, rtcp_ssrc, buf.len(), plain.len());
                    }
                }
                Err(e) => {
                    info!("[DBG:RTCP] decrypt FAILED user={} pt={} ssrc=0x{:08X}: {e}",
                        sender.user_id, rtcp_pt, rtcp_ssrc);
                }
            }
            return; // don't relay RTCP
        }

        // Decrypt SRTP → plaintext RTP
        let plaintext = {
            let mut ctx = sender.inbound_srtp.lock().unwrap();
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

        // Fan-out: relay to all other media-ready participants in the room
        let targets = room.other_participants(&sender.user_id);

        if is_detail {
            let target_info: Vec<String> = targets.iter()
                .filter(|t| t.is_media_ready())
                .map(|t| format!("{}@{}", t.user_id, t.get_address()
                    .map(|a| a.to_string()).unwrap_or("none".into())))
                .collect();
            info!("[DBG:RELAY] #{} from={} targets=[{}]",
                seq_num, sender.user_id, target_info.join(", "));
        }

        let mut relay_count = 0u32;
        for target in &targets {
            if !target.is_media_ready() { continue; }

            let addr = match target.get_address() {
                Some(a) => a,
                None => continue,
            };

            let encrypted = {
                let mut ctx = target.outbound_srtp.lock().unwrap();
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

// ============================================================================
// [DBG] RTP header parser (logging only)
// ============================================================================

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

/// Parse SSRC at offset 4 (RTCP) — for logging
fn parse_ssrc(buf: &[u8]) -> u32 {
    if buf.len() < 8 { return 0; }
    u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]])
}
