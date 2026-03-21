// author: kodeholic (powered by Claude)
//! Floor Control handlers — PTT request/release/ping/queue_pos + action dispatch

use std::sync::atomic::Ordering;

use crate::config;
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

    let priority = req.priority.unwrap_or(config::FLOOR_DEFAULT_PRIORITY);
    let now = current_ts();
    let actions = room.floor.request(user_id, priority, now);

    // Vec<FloorAction> 처리
    let mut response = Packet::ok(opcode::FLOOR_REQUEST, packet.pid, serde_json::json!({}));
    for action in &actions {
        match action {
            FloorAction::Granted { speaker, priority, duration_s } => {
                state.metrics.ptt_floor_granted.fetch_add(1, Ordering::Relaxed);
                state.metrics.ptt_speaker_switches.fetch_add(1, Ordering::Relaxed);
                room.audio_rewriter.switch_speaker(speaker);
                room.video_rewriter.switch_speaker(speaker);

                // PLI burst
                if let Some(participant) = room.get_participant(speaker) {
                    if participant.is_publish_ready() {
                        if let Some(pub_addr) = participant.publish.get_address() {
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

                // 요청자 본인이 grant된 경우 → 응답에 granted 포함
                if speaker == user_id {
                    response = Packet::ok(opcode::FLOOR_REQUEST, packet.pid, serde_json::json!({
                        "granted": true,
                        "speaker": speaker,
                        "priority": priority,
                        "duration": duration_s,
                    }));
                }

                // 전체에 FLOOR_TAKEN 브로드캐스트
                let taken = Packet::new(opcode::FLOOR_TAKEN, 0, serde_json::json!({
                    "room_id": room.id,
                    "speaker": speaker,
                    "priority": priority,
                }));
                broadcast_to_others(&room, user_id, &taken);
            }
            FloorAction::Revoked { prev_speaker, cause } => {
                state.metrics.ptt_floor_revoked.fetch_add(1, Ordering::Relaxed);
                if cause == "preempted" {
                    state.metrics.ptt_floor_preempted.fetch_add(1, Ordering::Relaxed);
                }
                // 선점된 사용자에게 REVOKE
                if let Some(p) = room.get_participant(prev_speaker) {
                    p.cancel_pli_burst();
                    let revoke = Packet::new(opcode::FLOOR_REVOKE, 0, serde_json::json!({
                        "room_id": room.id,
                        "cause": cause,
                    }));
                    let json = serde_json::to_string(&revoke).unwrap_or_default();
                    let _ = p.ws_tx.send(json);
                }
            }
            FloorAction::Queued { user_id: _queued_uid, position, priority, queue_size } => {
                state.metrics.ptt_floor_queued.fetch_add(1, Ordering::Relaxed);
                response = Packet::ok(opcode::FLOOR_REQUEST, packet.pid, serde_json::json!({
                    "queued": true,
                    "position": position,
                    "priority": priority,
                    "queue_size": queue_size,
                }));
            }
            FloorAction::Denied { reason, current_speaker } => {
                response = Packet::err(opcode::FLOOR_REQUEST, packet.pid, 2010,
                    &format!("{} (speaker={})", reason, current_speaker));
            }
            _ => {}
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

    let actions = room.floor.release(user_id);

    for action in &actions {
        match action {
            FloorAction::Released { .. } => {
                state.metrics.ptt_floor_released.fetch_add(1, Ordering::Relaxed);
                flush_ptt_silence(&room);
            }
            _ => {}
        }
    }

    apply_floor_actions(opcode::FLOOR_RELEASE, packet.pid, &actions, &room, user_id, state)
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
// FLOOR_QUEUE_POS — 큐 위치 조회
// ============================================================================

pub(super) fn handle_floor_queue_pos(session: &Session, state: &AppState, packet: &Packet) -> Packet {
    let req: FloorQueuePosMsg = match serde_json::from_value(packet.d.clone()) {
        Ok(r) => r,
        Err(_) => return Packet::err(opcode::FLOOR_QUEUE_POS, packet.pid, 3002, "invalid payload"),
    };

    let user_id = session.user_id.as_ref().unwrap();
    let room = match state.rooms.get(&req.room_id) {
        Ok(r) => r,
        Err(_) => return Packet::err(opcode::FLOOR_QUEUE_POS, packet.pid, 2001, "room not found"),
    };

    match room.floor.query_queue_position(user_id) {
        Some((position, priority)) => {
            Packet::ok(opcode::FLOOR_QUEUE_POS, packet.pid, serde_json::json!({
                "position": position,
                "priority": priority,
                "queue_size": room.floor.queue_size(),
            }))
        }
        None => {
            Packet::ok(opcode::FLOOR_QUEUE_POS, packet.pid, serde_json::json!({
                "position": 0,
                "priority": 0,
                "queue_size": room.floor.queue_size(),
            }))
        }
    }
}

// ============================================================================
// Vec<FloorAction> → 응답 패킷 + 브로드캐스트
// ============================================================================

pub(super) fn apply_floor_actions(
    op: u16,
    pid: u64,
    actions: &[FloorAction],
    room: &crate::room::room::Room,
    user_id: &str,
    state: &AppState,
) -> Packet {
    let mut response = Packet::ok(op, pid, serde_json::json!({}));

    for action in actions {
        match action {
            FloorAction::Granted { speaker, priority, duration_s } => {
                state.metrics.ptt_floor_granted.fetch_add(1, Ordering::Relaxed);
                state.metrics.ptt_speaker_switches.fetch_add(1, Ordering::Relaxed);
                room.audio_rewriter.switch_speaker(speaker);
                room.video_rewriter.switch_speaker(speaker);

                // PLI burst for new speaker
                if let Some(participant) = room.get_participant(speaker) {
                    if participant.is_publish_ready() {
                        if let Some(pub_addr) = participant.publish.get_address() {
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

                let taken = Packet::new(opcode::FLOOR_TAKEN, 0, serde_json::json!({
                    "room_id": room.id,
                    "speaker": speaker,
                    "priority": priority,
                }));
                broadcast_to_room(room, &taken);

                // queue pop에 의한 grant → 해당 사용자에게 개별 Granted 응답 전송
                if speaker != user_id {
                    state.metrics.ptt_floor_queue_pop.fetch_add(1, Ordering::Relaxed);
                    if let Some(p) = room.get_participant(speaker) {
                        let granted = Packet::ok(opcode::FLOOR_REQUEST, 0, serde_json::json!({
                            "granted": true,
                            "speaker": speaker,
                            "priority": priority,
                            "duration": duration_s,
                        }));
                        let json = serde_json::to_string(&granted).unwrap_or_default();
                        let _ = p.ws_tx.send(json);
                    }
                }
            }
            FloorAction::Denied { reason, current_speaker } => {
                response = Packet::err(op, pid, 2010,
                    &format!("{} (speaker={})", reason, current_speaker));
            }
            FloorAction::Released { prev_speaker } => {
                let idle = Packet::new(opcode::FLOOR_IDLE, 0, serde_json::json!({
                    "room_id": room.id,
                    "prev_speaker": prev_speaker,
                }));
                broadcast_to_room(room, &idle);
            }
            FloorAction::Revoked { prev_speaker, cause } => {
                state.metrics.ptt_floor_revoked.fetch_add(1, Ordering::Relaxed);
                if cause == "preempted" {
                    state.metrics.ptt_floor_preempted.fetch_add(1, Ordering::Relaxed);
                }
                if let Some(p) = room.get_participant(prev_speaker) {
                    p.cancel_pli_burst();
                    let revoke = Packet::new(opcode::FLOOR_REVOKE, 0, serde_json::json!({
                        "room_id": room.id,
                        "cause": cause,
                    }));
                    let json = serde_json::to_string(&revoke).unwrap_or_default();
                    let _ = p.ws_tx.send(json);
                }
                let idle = Packet::new(opcode::FLOOR_IDLE, 0, serde_json::json!({
                    "room_id": room.id,
                    "prev_speaker": prev_speaker,
                    "cause": cause,
                }));
                broadcast_to_room(room, &idle);
            }
            _ => {}
        }
    }

    response
}
