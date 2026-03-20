// author: kodeholic (powered by Claude)
//! Floor Control handlers — PTT request/release/ping + action dispatch

use std::sync::atomic::Ordering;

use crate::config::RoomMode;
use crate::room::participant::TrackKind;
use crate::room::floor::FloorAction;
use crate::signaling::message::*;
use crate::signaling::opcode;
use crate::state::AppState;
use crate::transport::udp::spawn_pli_burst;

use super::Session;
use super::helpers::*;

// ============================================================================
// FLOOR_REQUEST
// ============================================================================

pub(super) async fn handle_floor_request(session: &Session, state: &AppState, packet: &Packet) -> Packet {
    let req: FloorRequestMsg = match serde_json::from_value(packet.d.clone()) {
        Ok(r) => r,
        Err(_) => return Packet::err(opcode::FLOOR_REQUEST, packet.pid, 3002, "invalid payload"),
    };

    let user_id = session.user_id.as_ref().unwrap();
    let room = match state.rooms.get(&req.room_id) {
        Ok(r) => r,
        Err(_) => return Packet::err(opcode::FLOOR_REQUEST, packet.pid, 2001, "room not found"),
    };

    if room.mode != RoomMode::Ptt {
        return Packet::err(opcode::FLOOR_REQUEST, packet.pid, 2020, "not a PTT room");
    }

    let now = current_ts();
    let action = room.floor.request(user_id, now);
    let response = apply_floor_action(opcode::FLOOR_REQUEST, packet.pid, &action, &room, user_id);

    if let FloorAction::Granted { speaker } = &action {
        state.metrics.ptt_floor_granted.fetch_add(1, Ordering::Relaxed);
        state.metrics.ptt_speaker_switches.fetch_add(1, Ordering::Relaxed);
        room.audio_rewriter.switch_speaker(speaker);
        room.video_rewriter.switch_speaker(speaker);
        if let Some(participant) = room.get_participant(speaker) {
            if !participant.is_publish_ready() { /* SRTP not ready yet */ }
            else if let Some(pub_addr) = participant.publish.get_address() {
                let video_ssrc = {
                    let tracks = participant.tracks.lock().unwrap();
                    tracks.iter().find(|t| t.kind == TrackKind::Video).map(|t| t.ssrc)
                };
                if let Some(ssrc) = video_ssrc {
                    spawn_pli_burst(&participant, ssrc, pub_addr, state.udp_socket.clone(), &[0, 500, 1500], "FLOOR");
                }
            }
        }
    }

    response
}

// ============================================================================
// FLOOR_RELEASE
// ============================================================================

pub(super) async fn handle_floor_release(session: &Session, state: &AppState, packet: &Packet) -> Packet {
    let req: FloorReleaseMsg = match serde_json::from_value(packet.d.clone()) {
        Ok(r) => r,
        Err(_) => return Packet::err(opcode::FLOOR_RELEASE, packet.pid, 3002, "invalid payload"),
    };

    let user_id = session.user_id.as_ref().unwrap();
    let room = match state.rooms.get(&req.room_id) {
        Ok(r) => r,
        Err(_) => return Packet::err(opcode::FLOOR_RELEASE, packet.pid, 2001, "room not found"),
    };

    if room.mode != RoomMode::Ptt {
        return Packet::err(opcode::FLOOR_RELEASE, packet.pid, 2020, "not a PTT room");
    }

    let action = room.floor.release(user_id);

    if matches!(&action, FloorAction::Released { .. }) {
        state.metrics.ptt_floor_released.fetch_add(1, Ordering::Relaxed);
        flush_ptt_silence(&room);
    }

    apply_floor_action(opcode::FLOOR_RELEASE, packet.pid, &action, &room, user_id)
}

// ============================================================================
// FLOOR_PING
// ============================================================================

pub(super) fn handle_floor_ping(session: &Session, state: &AppState, packet: &Packet) -> Packet {
    let req: FloorPingMsg = match serde_json::from_value(packet.d.clone()) {
        Ok(r) => r,
        Err(_) => return Packet::err(opcode::FLOOR_PING, packet.pid, 3002, "invalid payload"),
    };

    let user_id = session.user_id.as_ref().unwrap();
    let room = match state.rooms.get(&req.room_id) {
        Ok(r) => r,
        Err(_) => return Packet::err(opcode::FLOOR_PING, packet.pid, 2001, "room not found"),
    };

    let now = current_ts();
    let action = room.floor.ping(user_id, now);
    match action {
        FloorAction::PingOk => Packet::ok(opcode::FLOOR_PING, packet.pid, serde_json::json!({})),
        _ => Packet::err(opcode::FLOOR_PING, packet.pid, 2021, "not current speaker"),
    }
}

// ============================================================================
// FloorAction → 응답 패킷 + 브로드캐스트
// ============================================================================

pub(super) fn apply_floor_action(
    op: u16,
    pid: u64,
    action: &FloorAction,
    room: &crate::room::room::Room,
    user_id: &str,
) -> Packet {
    match action {
        FloorAction::Granted { speaker } => {
            let taken = Packet::new(opcode::FLOOR_TAKEN, 0, serde_json::json!({ "room_id": room.id, "speaker": speaker }));
            broadcast_to_others(room, user_id, &taken);
            Packet::ok(op, pid, serde_json::json!({ "granted": true, "speaker": speaker }))
        }
        FloorAction::Denied { reason, current_speaker } => {
            Packet::err(op, pid, 2010, &format!("{} (speaker={})", reason, current_speaker))
        }
        FloorAction::Released { prev_speaker } => {
            let idle = Packet::new(opcode::FLOOR_IDLE, 0, serde_json::json!({ "room_id": room.id, "prev_speaker": prev_speaker }));
            broadcast_to_room(room, &idle);
            Packet::ok(op, pid, serde_json::json!({}))
        }
        FloorAction::Revoked { prev_speaker, cause } => {
            let revoke = Packet::new(opcode::FLOOR_REVOKE, 0, serde_json::json!({ "room_id": room.id, "cause": cause }));
            if let Some(p) = room.get_participant(prev_speaker) {
                let json = serde_json::to_string(&revoke).unwrap_or_default();
                let _ = p.ws_tx.send(json);
            }
            let idle = Packet::new(opcode::FLOOR_IDLE, 0, serde_json::json!({ "room_id": room.id, "prev_speaker": prev_speaker, "cause": cause }));
            broadcast_to_room(room, &idle);
            Packet::ok(op, pid, serde_json::json!({}))
        }
        _ => Packet::ok(op, pid, serde_json::json!({})),
    }
}
