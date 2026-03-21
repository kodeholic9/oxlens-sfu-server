// author: kodeholic (powered by Claude)
//! WebSocket handler — signaling lifecycle (2PC / SDP-free)
//!
//! Connection flow:
//!   1. Server sends HELLO (heartbeat_interval)
//!   2. Client sends IDENTIFY (token) → Server responds with user_id
//!   3. Client sends ROOM_JOIN (room_id)
//!      → Server creates Participant (pub_ufrag, sub_ufrag)
//!      → Server registers in RoomHub (3 indices × 2 sessions)
//!      → Server responds with server_config (ICE, DTLS, codecs, extmap)
//!   4. Client builds fake SDP locally → ICE → STUN → DTLS → SRTP
//!   5. Client sends PUBLISH_TRACKS (ssrc, kind per track)
//!   6. Server broadcasts TRACKS_UPDATE to other participants
//!   7. Client sends ROOM_LEAVE or disconnects → cleanup

mod room_ops;
mod track_ops;
mod floor_ops;
mod admin;
pub(crate) mod helpers;
mod telemetry;

// Re-export public entry points (lib.rs에서 사용)
pub use admin::admin_ws_handler;

use axum::{
    extract::{State, WebSocketUpgrade, ws::{Message, WebSocket}},
    response::IntoResponse,
};
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::mpsc;
use tokio::time::{Duration, Instant, interval_at};
use tracing::{debug, info, warn};

use crate::config;
use crate::signaling::message::*;
use crate::signaling::opcode;
use crate::state::AppState;

use helpers::current_ts;

// ============================================================================
// Session (per-WS connection)
// ============================================================================

pub(super) struct Session {
    pub(super) user_id:      Option<String>,
    pub(super) current_room: Option<String>,
    /// publish ufrag (cleanup key)
    pub(super) pub_ufrag:    Option<String>,
    /// subscribe ufrag (cleanup key)
    pub(super) sub_ufrag:    Option<String>,
    pub(super) server_pid:   AtomicU64,
    pub(super) ack_miss:     AtomicU64,
    pub(super) ws_tx:        mpsc::UnboundedSender<String>,
}

impl Session {
    fn new(ws_tx: mpsc::UnboundedSender<String>) -> Self {
        Self {
            user_id:      None,
            current_room: None,
            pub_ufrag:    None,
            sub_ufrag:    None,
            server_pid:   AtomicU64::new(1),
            ack_miss:     AtomicU64::new(0),
            ws_tx,
        }
    }

    pub(super) fn next_pid(&self) -> u64 {
        self.server_pid.fetch_add(1, Ordering::Relaxed)
    }

    pub(super) fn is_authenticated(&self) -> bool {
        self.user_id.is_some()
    }
}

// ============================================================================
// WS entry point
// ============================================================================

pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_connection(socket, state))
}

async fn handle_connection(mut socket: WebSocket, state: AppState) {
    let (ws_tx, mut ws_rx) = mpsc::unbounded_channel::<String>();
    let mut session = Session::new(ws_tx.clone());

    // Send HELLO
    let hello = Packet::new(
        opcode::HELLO,
        session.next_pid(),
        serde_json::to_value(HelloEvent {
            heartbeat_interval: config::HEARTBEAT_INTERVAL_MS,
        }).unwrap(),
    );
    if send_packet(&mut socket, &hello).await.is_err() {
        return;
    }

    // Heartbeat timeout tracking
    let mut last_activity = Instant::now();
    let hb_timeout = Duration::from_millis(config::HEARTBEAT_TIMEOUT_MS);
    let check_interval = Duration::from_millis(config::HEARTBEAT_INTERVAL_MS);
    let mut hb_timer = interval_at(Instant::now() + check_interval, check_interval);

    loop {
        tokio::select! {
            Some(json) = ws_rx.recv() => {
                if socket.send(Message::Text(json.into())).await.is_err() {
                    break;
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        last_activity = Instant::now();
                        let text_str: &str = &text;
                        match serde_json::from_str::<Packet>(text_str) {
                            Ok(packet) => {
                                if let Some(resp) = dispatch(&mut session, &state, packet).await {
                                    if send_packet(&mut socket, &resp).await.is_err() {
                                        break;
                                    }
                                }
                            }
                            Err(e) => warn!("invalid packet: {e}"),
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(e)) => { warn!("ws error: {e}"); break; }
                    _ => { last_activity = Instant::now(); }
                }
            }
            _ = hb_timer.tick() => {
                if last_activity.elapsed() > hb_timeout {
                    warn!("heartbeat timeout user={:?} elapsed={:.1}s",
                        session.user_id, last_activity.elapsed().as_secs_f32());
                    break;
                }
            }
        }
    }

    room_ops::cleanup(&session, &state).await;
    info!("connection closed: {:?}", session.user_id);
}

async fn send_packet(socket: &mut WebSocket, packet: &Packet) -> Result<(), ()> {
    let json = serde_json::to_string(packet).map_err(|_| ())?;
    socket.send(Message::Text(json.into())).await.map_err(|_| ())
}

// ============================================================================
// Dispatch
// ============================================================================

async fn dispatch(session: &mut Session, state: &AppState, packet: Packet) -> Option<Packet> {
    if packet.is_response() {
        debug!("ACK received pid={}", packet.pid);
        session.ack_miss.store(0, Ordering::Relaxed);
        return None;
    }

    if packet.op != opcode::HEARTBEAT && packet.op != opcode::IDENTIFY {
        if !session.is_authenticated() {
            return Some(Packet::err(packet.op, packet.pid, 1001, "not authenticated"));
        }
    }

    match packet.op {
        opcode::HEARTBEAT       => {
            // 좀비 reaper용 last_seen 갱신 (room 에 있는 경우)
            if let (Some(room_id), Some(user_id)) = (&session.current_room, &session.user_id) {
                if let Ok(room) = state.rooms.get(room_id) {
                    if let Some(p) = room.get_participant(user_id) {
                        p.touch(current_ts());
                    }
                }
            }
            Some(Packet::ok(opcode::HEARTBEAT, packet.pid, serde_json::json!({})))
        }
        opcode::IDENTIFY        => Some(room_ops::handle_identify(session, state, &packet)),
        opcode::ROOM_LIST       => Some(room_ops::handle_room_list(state, &packet)),
        opcode::ROOM_CREATE     => Some(room_ops::handle_room_create(state, &packet)),
        opcode::ROOM_JOIN       => Some(room_ops::handle_room_join(session, state, &packet).await),
        opcode::ROOM_LEAVE      => Some(room_ops::handle_room_leave(session, state, &packet).await),
        opcode::PUBLISH_TRACKS  => Some(track_ops::handle_publish_tracks(session, state, &packet).await),
        opcode::TRACKS_ACK      => Some(track_ops::handle_tracks_ack(session, state, &packet).await),
        opcode::MUTE_UPDATE     => Some(track_ops::handle_mute_update(session, state, &packet).await),
        opcode::CAMERA_READY    => Some(track_ops::handle_camera_ready(session, state, &packet).await),
        opcode::MESSAGE         => Some(room_ops::handle_message(session, state, &packet).await),
        opcode::TELEMETRY      => { telemetry::handle_telemetry(session, &packet); None }
        opcode::ROOM_SYNC       => Some(room_ops::handle_room_sync(session, state, &packet).await),
        // Floor Control (MCPTT/MBCP)
        opcode::FLOOR_REQUEST   => Some(floor_ops::handle_floor_request(session, state, &packet).await),
        opcode::FLOOR_RELEASE   => Some(floor_ops::handle_floor_release(session, state, &packet).await),
        opcode::FLOOR_PING      => Some(floor_ops::handle_floor_ping(session, state, &packet)),
        opcode::FLOOR_QUEUE_POS => Some(floor_ops::handle_floor_queue_pos(session, state, &packet)),
        opcode::SUBSCRIBE_LAYER => Some(track_ops::handle_subscribe_layer(session, state, &packet).await),
        _ => {
            warn!("unknown opcode: {}", packet.op);
            Some(Packet::err(packet.op, packet.pid, 3001, "invalid opcode"))
        }
    }
}
