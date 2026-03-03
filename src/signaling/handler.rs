// author: kodeholic (powered by Claude)
//! WebSocket handler — signaling lifecycle
//!
//! Connection flow:
//!   1. Server sends HELLO (heartbeat_interval)
//!   2. Client sends IDENTIFY (token) → Server responds with user_id
//!   3. Client sends ROOM_JOIN (room_id, sdp_offer)
//!      → Server parses SDP Offer (codecs, extmap, SSRC)
//!      → Server creates Participant (ufrag, ice_pwd)
//!      → Server registers in RoomHub (3 indices)
//!      → Server builds SDP Answer (ICE-Lite, passive DTLS, codec mirror)
//!      → Server responds with sdp_answer + participants
//!   4. Browser initiates ICE → STUN → DTLS → SRTP (media path)
//!   5. Client sends ROOM_LEAVE or disconnects → cleanup

use axum::{
    extract::{State, WebSocketUpgrade, ws::{Message, WebSocket}},
    response::IntoResponse,
};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::config;
use crate::room::participant::Participant;
use crate::signaling::message::*;
use crate::signaling::opcode;
use crate::state::AppState;
use crate::transport::ice::IceCredentials;
use crate::transport::sdp;

// ============================================================================
// Session (per-WS connection)
// ============================================================================

struct Session {
    user_id:      Option<String>,
    current_room: Option<String>,
    ufrag:        Option<String>,   // RoomHub cleanup key
    server_pid:   AtomicU64,
    ack_miss:     AtomicU64,
    /// Send signaling messages back to this WS
    ws_tx:        mpsc::UnboundedSender<String>,
}

impl Session {
    fn new(ws_tx: mpsc::UnboundedSender<String>) -> Self {
        Self {
            user_id:      None,
            current_room: None,
            ufrag:        None,
            server_pid:   AtomicU64::new(1),
            ack_miss:     AtomicU64::new(0),
            ws_tx,
        }
    }

    fn next_pid(&self) -> u64 {
        self.server_pid.fetch_add(1, Ordering::Relaxed)
    }

    fn is_authenticated(&self) -> bool {
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

    // Single loop with select: no split needed at our scale (20 participants).
    // Server-initiated events go through ws_tx → checked in the loop.

    loop {
        tokio::select! {
            // Outbound: server events queued via ws_tx
            Some(json) = ws_rx.recv() => {
                if socket.send(Message::Text(json.into())).await.is_err() {
                    break;
                }
            }
            // Inbound: client messages
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
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
                    _ => {}
                }
            }
        }
    }

    // Cleanup on disconnect
    cleanup(&session, &state).await;
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
    // ACK from client (response to our event)
    if packet.is_response() {
        debug!("ACK received pid={}", packet.pid);
        session.ack_miss.store(0, Ordering::Relaxed);
        return None;
    }

    // Auth gate: only HEARTBEAT and IDENTIFY allowed before auth
    if packet.op != opcode::HEARTBEAT && packet.op != opcode::IDENTIFY {
        if !session.is_authenticated() {
            return Some(Packet::err(packet.op, packet.pid, 1001, "not authenticated"));
        }
    }

    match packet.op {
        opcode::HEARTBEAT    => Some(Packet::ok(opcode::HEARTBEAT, packet.pid, serde_json::json!({}))),
        opcode::IDENTIFY     => Some(handle_identify(session, state, &packet)),
        opcode::ROOM_LIST    => Some(handle_room_list(state, &packet)),
        opcode::ROOM_CREATE  => Some(handle_room_create(state, &packet)),
        opcode::ROOM_JOIN    => Some(handle_room_join(session, state, &packet).await),
        opcode::ROOM_LEAVE   => Some(handle_room_leave(session, state, &packet).await),
        opcode::SDP_OFFER    => Some(handle_sdp_offer(session, state, &packet)),
        opcode::ICE_CANDIDATE => {
            debug!("ICE_CANDIDATE (trickle ICE — ignored in ICE-Lite)");
            Some(Packet::ok(opcode::ICE_CANDIDATE, packet.pid, serde_json::json!({})))
        }
        opcode::MESSAGE      => Some(handle_message(session, state, &packet).await),
        _ => {
            warn!("unknown opcode: {}", packet.op);
            Some(Packet::err(packet.op, packet.pid, 3001, "invalid opcode"))
        }
    }
}

// ============================================================================
// IDENTIFY
// ============================================================================

fn handle_identify(session: &mut Session, _state: &AppState, packet: &Packet) -> Packet {
    let req: IdentifyRequest = match serde_json::from_value(packet.d.clone()) {
        Ok(r) => r,
        Err(_) => return Packet::err(opcode::IDENTIFY, packet.pid, 3002, "invalid payload"),
    };

    // TODO: token verification (for now accept anything)
    // 클라이언트가 user_id 지정 → 수용, 없으면 랜덤 생성
    let user_id = req.user_id
        .filter(|id| !id.trim().is_empty())
        .unwrap_or_else(|| format!("U{:03}", rand_u16() % 1000));
    session.user_id = Some(user_id.clone());

    info!("IDENTIFY ok user={}", user_id);

    Packet::ok(opcode::IDENTIFY, packet.pid, serde_json::json!({
        "user_id": user_id,
    }))
}

// ============================================================================
// ROOM_LIST
// ============================================================================

fn handle_room_list(state: &AppState, packet: &Packet) -> Packet {
    let rooms: Vec<serde_json::Value> = state.rooms.rooms
        .iter()
        .map(|entry| {
            let room = entry.value();
            serde_json::json!({
                "room_id": room.id,
                "name": room.name,
                "capacity": room.capacity,
                "participants": room.participant_count(),
            })
        })
        .collect();

    debug!("ROOM_LIST count={}", rooms.len());

    Packet::ok(opcode::ROOM_LIST, packet.pid, serde_json::json!({
        "rooms": rooms,
    }))
}

// ============================================================================
// ROOM_CREATE
// ============================================================================

fn handle_room_create(state: &AppState, packet: &Packet) -> Packet {
    let req: RoomCreateRequest = match serde_json::from_value(packet.d.clone()) {
        Ok(r) => r,
        Err(_) => return Packet::err(opcode::ROOM_CREATE, packet.pid, 3002, "invalid payload"),
    };

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    let room = state.rooms.create(req.name.clone(), req.capacity, now);
    info!("ROOM_CREATE id={} name={}", room.id, room.name);

    Packet::ok(opcode::ROOM_CREATE, packet.pid, serde_json::json!({
        "room_id": room.id,
        "name": room.name,
        "capacity": room.capacity,
    }))
}

// ============================================================================
// ROOM_JOIN — the core signaling path
// ============================================================================

async fn handle_room_join(session: &mut Session, state: &AppState, packet: &Packet) -> Packet {
    let req: RoomJoinRequest = match serde_json::from_value(packet.d.clone()) {
        Ok(r) => r,
        Err(_) => return Packet::err(opcode::ROOM_JOIN, packet.pid, 3002, "invalid payload"),
    };

    let user_id = session.user_id.clone().unwrap();

    // Parse SDP Offer
    let parsed_offer = match sdp::parse_offer(&req.sdp_offer) {
        Ok(p) => p,
        Err(e) => {
            warn!("SDP parse failed user={}: {}", user_id, e);
            return Packet::err(opcode::ROOM_JOIN, packet.pid, 4001, "sdp parse error");
        }
    };

    // Get room
    let room = match state.rooms.get(&req.room_id) {
        Ok(r) => r,
        Err(_) => return Packet::err(opcode::ROOM_JOIN, packet.pid, 2001, "room not found"),
    };

    // Generate ICE credentials for this participant
    let ice = IceCredentials::new();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    // Create Participant
    let participant = Arc::new(Participant::new(
        user_id.clone(),
        req.room_id.clone(),
        ice.ufrag.clone(),
        ice.pwd.clone(),
        session.ws_tx.clone(),
        now,
    ));

    // Register in RoomHub (participants + ufrag_index)
    if let Err(e) = state.rooms.add_participant(&req.room_id, Arc::clone(&participant)) {
        return Packet::err(opcode::ROOM_JOIN, packet.pid, e.code(), &e.to_string());
    }

    session.current_room = Some(req.room_id.clone());
    session.ufrag = Some(ice.ufrag.clone());

    // Build SDP Answer
    let sdp_answer = sdp::build_answer(
        &parsed_offer,
        &state.cert.fingerprint,
        &ice.ufrag,
        &ice.pwd,
        config::UDP_PORT,
    );

    info!("ROOM_JOIN user={} room={} ufrag={} sections={}",
        user_id, req.room_id, ice.ufrag, parsed_offer.sections.len());

    // Notify existing participants
    let event = Packet::new(
        opcode::ROOM_EVENT,
        0, // event pid will be set by receiver
        serde_json::to_value(RoomEventPayload {
            event_type: "participant_joined".to_string(),
            room_id: req.room_id.clone(),
            user_id: Some(user_id.clone()),
        }).unwrap(),
    );
    broadcast_to_others(&room, &user_id, &event);

    // Current member list
    let members: Vec<String> = room.member_ids();

    Packet::ok(opcode::ROOM_JOIN, packet.pid, serde_json::json!({
        "room_id": req.room_id,
        "sdp_answer": sdp_answer,
        "participants": members,
    }))
}

// ============================================================================
// ROOM_LEAVE
// ============================================================================

async fn handle_room_leave(session: &mut Session, state: &AppState, packet: &Packet) -> Packet {
    let req: RoomLeaveRequest = match serde_json::from_value(packet.d.clone()) {
        Ok(r) => r,
        Err(_) => return Packet::err(opcode::ROOM_LEAVE, packet.pid, 3002, "invalid payload"),
    };

    let user_id = session.user_id.as_ref().unwrap();

    match state.rooms.remove_participant(&req.room_id, user_id) {
        Ok(_p) => {
            info!("ROOM_LEAVE user={} room={}", user_id, req.room_id);

            // Notify remaining participants
            if let Ok(room) = state.rooms.get(&req.room_id) {
                let event = Packet::new(
                    opcode::ROOM_EVENT,
                    0,
                    serde_json::to_value(RoomEventPayload {
                        event_type: "participant_left".to_string(),
                        room_id: req.room_id.clone(),
                        user_id: Some(user_id.clone()),
                    }).unwrap(),
                );
                broadcast_to_room(&room, &event);
            }

            session.current_room = None;
            session.ufrag = None;

            Packet::ok(opcode::ROOM_LEAVE, packet.pid, serde_json::json!({}))
        }
        Err(e) => Packet::err(opcode::ROOM_LEAVE, packet.pid, e.code(), &e.to_string()),
    }
}

// ============================================================================
// SDP_OFFER (renegotiation)
// ============================================================================

fn handle_sdp_offer(_session: &Session, _state: &AppState, packet: &Packet) -> Packet {
    // TODO: Phase 4 — renegotiation for adding/removing tracks
    debug!("SDP_OFFER received (renegotiation — not yet implemented)");
    Packet::ok(opcode::SDP_OFFER, packet.pid, serde_json::json!({
        "sdp_answer": "TODO"
    }))
}

// ============================================================================
// MESSAGE (chat relay)
// ============================================================================

async fn handle_message(session: &Session, state: &AppState, packet: &Packet) -> Packet {
    let req: MessageRequest = match serde_json::from_value(packet.d.clone()) {
        Ok(r) => r,
        Err(_) => return Packet::err(opcode::MESSAGE, packet.pid, 3002, "invalid payload"),
    };

    let user_id = session.user_id.as_ref().unwrap();

    if let Ok(room) = state.rooms.get(&req.room_id) {
        let msg_event = Packet::new(
            opcode::MESSAGE_EVENT,
            0,
            serde_json::json!({
                "room_id": req.room_id,
                "user_id": user_id,
                "content": req.content,
            }),
        );
        broadcast_to_others(&room, user_id, &msg_event);
    }

    Packet::ok(opcode::MESSAGE, packet.pid, serde_json::json!({
        "msg_id": uuid::Uuid::new_v4().to_string(),
    }))
}

// ============================================================================
// Cleanup (WS disconnect)
// ============================================================================

async fn cleanup(session: &Session, state: &AppState) {
    if let (Some(room_id), Some(user_id)) = (&session.current_room, &session.user_id) {
        // Notify others before removing
        if let Ok(room) = state.rooms.get(room_id) {
            let event = Packet::new(
                opcode::ROOM_EVENT,
                0,
                serde_json::to_value(RoomEventPayload {
                    event_type: "participant_left".to_string(),
                    room_id: room_id.clone(),
                    user_id: Some(user_id.clone()),
                }).unwrap(),
            );
            broadcast_to_others(&room, user_id, &event);
        }

        if let Err(e) = state.rooms.remove_participant(room_id, user_id) {
            warn!("cleanup error: {e}");
        }
        debug!("cleanup done user={} room={}", user_id, room_id);
    }
}

// ============================================================================
// Broadcast helpers
// ============================================================================

fn broadcast_to_others(room: &crate::room::room::Room, exclude: &str, packet: &Packet) {
    let json = match serde_json::to_string(packet) {
        Ok(j) => j,
        Err(_) => return,
    };
    for entry in room.participants.iter() {
        if entry.key() != exclude {
            let _ = entry.value().ws_tx.send(json.clone());
        }
    }
}

fn broadcast_to_room(room: &crate::room::room::Room, packet: &Packet) {
    let json = match serde_json::to_string(packet) {
        Ok(j) => j,
        Err(_) => return,
    };
    for entry in room.participants.iter() {
        let _ = entry.value().ws_tx.send(json.clone());
    }
}

/// 랜덤 u16 생성 (getrandom 기반)
fn rand_u16() -> u16 {
    let mut buf = [0u8; 2];
    getrandom::fill(&mut buf).expect("getrandom failed");
    u16::from_le_bytes(buf)
}
