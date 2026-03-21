// author: kodeholic (powered by Claude)
//! Room lifecycle handlers — identify, room CRUD, message, cleanup

use std::sync::Arc;
use std::sync::atomic::Ordering;
use tracing::{debug, info, warn};

use crate::config;
use crate::config::RoomMode;
use crate::room::participant::Participant;
use crate::signaling::message::*;
use crate::signaling::opcode;
use crate::state::AppState;
use crate::transport::ice::IceCredentials;

use super::Session;
use super::helpers::*;

// ============================================================================
// IDENTIFY
// ============================================================================

pub(super) fn handle_identify(session: &mut Session, _state: &AppState, packet: &Packet) -> Packet {
    let req: IdentifyRequest = match serde_json::from_value(packet.d.clone()) {
        Ok(r) => r,
        Err(_) => return Packet::err(opcode::IDENTIFY, packet.pid, 3002, "invalid payload"),
    };

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

pub(super) fn handle_room_list(state: &AppState, packet: &Packet) -> Packet {
    let rooms: Vec<serde_json::Value> = state.rooms.rooms
        .iter()
        .map(|entry| {
            let room = entry.value();
            serde_json::json!({
                "room_id": room.id,
                "name": room.name,
                "capacity": room.capacity,
                "mode": room.mode.to_string(),
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

pub(super) fn handle_room_create(state: &AppState, packet: &Packet) -> Packet {
    let req: RoomCreateRequest = match serde_json::from_value(packet.d.clone()) {
        Ok(r) => r,
        Err(_) => return Packet::err(opcode::ROOM_CREATE, packet.pid, 3002, "invalid payload"),
    };

    let now = current_ts();
    let mode = req.mode.unwrap_or(RoomModeField::Conference).to_config();
    let simulcast = req.simulcast.unwrap_or(false);
    let room = state.rooms.create(req.name.clone(), req.capacity, mode, now, simulcast);
    info!("ROOM_CREATE id={} name={} mode={} simulcast={}", room.id, room.name, room.mode, room.simulcast_enabled);

    Packet::ok(opcode::ROOM_CREATE, packet.pid, serde_json::json!({
        "room_id": room.id,
        "name": room.name,
        "capacity": room.capacity,
        "mode": room.mode.to_string(),
        "simulcast": room.simulcast_enabled,
    }))
}

// ============================================================================
// ROOM_JOIN — server_config 응답 (SDP-free)
// ============================================================================

pub(super) async fn handle_room_join(session: &mut Session, state: &AppState, packet: &Packet) -> Packet {
    let req: RoomJoinRequest = match serde_json::from_value(packet.d.clone()) {
        Ok(r) => r,
        Err(_) => return Packet::err(opcode::ROOM_JOIN, packet.pid, 3002, "invalid payload"),
    };

    let user_id = session.user_id.clone().unwrap();

    let room = match state.rooms.get(&req.room_id) {
        Ok(r) => r,
        Err(_) => return Packet::err(opcode::ROOM_JOIN, packet.pid, 2001, "room not found"),
    };

    let pub_ice = IceCredentials::new();
    let sub_ice = IceCredentials::new();
    let now = current_ts();

    let participant = Arc::new(Participant::new(
        user_id.clone(),
        req.room_id.clone(),
        pub_ice.ufrag.clone(),
        pub_ice.pwd.clone(),
        sub_ice.ufrag.clone(),
        sub_ice.pwd.clone(),
        session.ws_tx.clone(),
        now,
    ));

    if let Err(e) = state.rooms.add_participant(&req.room_id, Arc::clone(&participant)) {
        return Packet::err(opcode::ROOM_JOIN, packet.pid, e.code(), &e.to_string());
    }

    session.current_room = Some(req.room_id.clone());
    session.pub_ufrag = Some(pub_ice.ufrag.clone());
    session.sub_ufrag = Some(sub_ice.ufrag.clone());

    // 재입장 방어: cleanup 실패/서버 크래시 등으로 stale entry가 남아있을 수 있음
    purge_subscribe_layers(&room, &user_id);

    info!("ROOM_JOIN user={} room={} pub_ufrag={} sub_ufrag={}",
        user_id, req.room_id, pub_ice.ufrag, sub_ice.ufrag);

    let sim_enabled = room.simulcast_enabled;
    let existing_tracks = collect_subscribe_tracks(&room, &user_id);

    let per_user: Vec<String> = room.other_participants(&user_id)
        .iter()
        .map(|p| format!("{}({})", p.user_id, p.get_tracks().len()))
        .collect();
    info!("ROOM_JOIN user={} room={} existing_tracks={} from=[{}]",
        user_id, req.room_id, existing_tracks.len(), per_user.join(", "));

    let event = Packet::new(
        opcode::ROOM_EVENT,
        0,
        serde_json::to_value(RoomEventPayload {
            event_type: "participant_joined".to_string(),
            room_id: req.room_id.clone(),
            user_id: Some(user_id.clone()),
        }).unwrap(),
    );
    broadcast_to_others(&room, &user_id, &event);

    push_admin_snapshot(state);

    let members: Vec<String> = room.member_ids();

    let mut response = serde_json::json!({
        "room_id": req.room_id,
        "mode": room.mode.to_string(),
        "participants": members,
        "server_config": {
            "ice": {
                "publish_ufrag": pub_ice.ufrag,
                "publish_pwd": pub_ice.pwd,
                "subscribe_ufrag": sub_ice.ufrag,
                "subscribe_pwd": sub_ice.pwd,
                "ip": state.public_ip,
                "port": state.udp_port,
            },
            "dtls": {
                "fingerprint": state.cert.fingerprint,
                "setup": "passive",
            },
            "codecs": server_codec_policy(),
            "extmap": server_extmap_policy(state.bwe_mode, sim_enabled),
            "max_bitrate_bps": config::resolve_remb_bitrate(),
        },
        "tracks": existing_tracks,
        "simulcast": {
            "enabled": sim_enabled,
        },
    });

    if room.mode == RoomMode::Ptt {
        response["ptt_virtual_ssrc"] = serde_json::json!({
            "audio": room.audio_rewriter.virtual_ssrc(),
            "video": room.video_rewriter.virtual_ssrc(),
        });
        let queue_snap: Vec<serde_json::Value> = room.floor.queue_snapshot()
            .iter().map(|(uid, pri, pos)| serde_json::json!({
                "user_id": uid, "priority": pri, "position": pos
            })).collect();
        response["floor"] = serde_json::json!({
            "speaker": room.floor.current_speaker(),
            "speaker_priority": room.floor.current_speaker_priority(),
            "queue": queue_snap,
        });
    }

    Packet::ok(opcode::ROOM_JOIN, packet.pid, response)
}

// ============================================================================
// ROOM_SYNC — 참여자+트랙+floor 전체 동기화 (클라이언트 폴링)
// ============================================================================

pub(super) async fn handle_room_sync(session: &Session, state: &AppState, packet: &Packet) -> Packet {
    let user_id = match &session.user_id {
        Some(id) => id,
        None => return Packet::err(opcode::ROOM_SYNC, packet.pid, 2003, "not identified"),
    };
    let room_id = match &session.current_room {
        Some(id) => id,
        None => return Packet::err(opcode::ROOM_SYNC, packet.pid, 2004, "not in room"),
    };
    let room = match state.rooms.get(room_id) {
        Ok(r) => r,
        Err(_) => return Packet::err(opcode::ROOM_SYNC, packet.pid, 2001, "room not found"),
    };

    let subscribe_tracks: Vec<serde_json::Value> = if room.mode == RoomMode::Ptt {
        vec![
            serde_json::json!({
                "kind": "audio",
                "ssrc": room.audio_rewriter.virtual_ssrc(),
                "track_id": "ptt-audio",
                "virtual": true,
            }),
            serde_json::json!({
                "kind": "video",
                "ssrc": room.video_rewriter.virtual_ssrc(),
                "track_id": "ptt-video",
                "virtual": true,
            }),
        ]
    } else {
        collect_subscribe_tracks(&room, user_id)
    };

    let participants: Vec<String> = room.member_ids();

    let queue_snap: Vec<serde_json::Value> = room.floor.queue_snapshot()
        .iter().map(|(uid, pri, pos)| serde_json::json!({
            "user_id": uid, "priority": pri, "position": pos
        })).collect();
    let floor = serde_json::json!({
        "speaker": room.floor.current_speaker(),
        "speaker_priority": room.floor.current_speaker_priority(),
        "queue": queue_snap,
    });

    debug!("ROOM_SYNC user={} room={} participants={} tracks={}",
        user_id, room_id, participants.len(), subscribe_tracks.len());

    Packet::ok(opcode::ROOM_SYNC, packet.pid, serde_json::json!({
        "room_id": room_id,
        "mode": room.mode.to_string(),
        "participants": participants,
        "subscribe_tracks": subscribe_tracks,
        "floor": floor,
        "total": participants.len(),
    }))
}

// ============================================================================
// ROOM_LEAVE
// ============================================================================

pub(super) async fn handle_room_leave(session: &mut Session, state: &AppState, packet: &Packet) -> Packet {
    let req: RoomLeaveRequest = match serde_json::from_value(packet.d.clone()) {
        Ok(r) => r,
        Err(_) => return Packet::err(opcode::ROOM_LEAVE, packet.pid, 3002, "invalid payload"),
    };

    let user_id = session.user_id.as_ref().unwrap();

    if let Ok(room) = state.rooms.get(&req.room_id) {
        if let Some(p) = room.get_participant(user_id) {
            p.cancel_pli_burst();
        }
        if room.mode == RoomMode::Ptt {
            let actions = room.floor.on_participant_leave(user_id);
            if !actions.is_empty() {
                super::floor_ops::apply_floor_actions(opcode::FLOOR_RELEASE, 0, &actions, &room, user_id, state);
                flush_ptt_silence(&room);
            }
        }
    }

    match state.rooms.remove_participant(&req.room_id, user_id) {
        Ok(p) => {
            info!("ROOM_LEAVE user={} room={}", user_id, req.room_id);

            if let Ok(room) = state.rooms.get(&req.room_id) {
                // simulcast: 퇴장한 유저의 subscribe_layers entry 정리 (재입장 시 stale rewriter 방지)
                purge_subscribe_layers(&room, user_id);

                let tracks = p.get_tracks();
                let vssrc_leave = p.simulcast_video_ssrc.load(Ordering::Relaxed);
                let remove_tracks = build_remove_tracks(&tracks, user_id, room.simulcast_enabled, vssrc_leave);
                if !remove_tracks.is_empty() {
                    let tracks_event = Packet::new(
                        opcode::TRACKS_UPDATE,
                        0,
                        serde_json::json!({
                            "action": "remove",
                            "tracks": remove_tracks,
                        }),
                    );
                    broadcast_to_room(&room, &tracks_event);
                }

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
            session.pub_ufrag = None;
            session.sub_ufrag = None;

            push_admin_snapshot(state);

            Packet::ok(opcode::ROOM_LEAVE, packet.pid, serde_json::json!({}))
        }
        Err(e) => Packet::err(opcode::ROOM_LEAVE, packet.pid, e.code(), &e.to_string()),
    }
}

// ============================================================================
// MESSAGE (chat relay)
// ============================================================================

pub(super) async fn handle_message(session: &Session, state: &AppState, packet: &Packet) -> Packet {
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

pub(super) async fn cleanup(session: &Session, state: &AppState) {
    if let (Some(room_id), Some(user_id)) = (&session.current_room, &session.user_id) {
        // Room 1회 조회 — 이후 모든 작업에서 재사용 (Arc clone, DashMap 락 없음)
        if let Ok(room) = state.rooms.get(room_id) {
            // 1~3. participant 1회 조회 → PLI cancel + 트랙 수집
            let (tracks, sim_vssrc) = if let Some(p) = room.get_participant(user_id) {
                p.cancel_pli_burst();
                (p.get_tracks(), p.simulcast_video_ssrc.load(Ordering::Relaxed))
            } else {
                (vec![], 0)
            };

            // PTT floor auto-release
            if room.mode == RoomMode::Ptt {
                let actions = room.floor.on_participant_leave(user_id);
                if !actions.is_empty() {
                    super::floor_ops::apply_floor_actions(opcode::FLOOR_RELEASE, 0, &actions, &room, user_id, state);
                    flush_ptt_silence(&room);
                }
            }

            // 3.5. simulcast: 퇴장한 유저의 subscribe_layers entry 정리
            purge_subscribe_layers(&room, user_id);

            // 4. Remove tracks + leave event broadcast
            let remove_tracks = build_remove_tracks(&tracks, user_id, room.simulcast_enabled, sim_vssrc);
            if !remove_tracks.is_empty() {
                let tracks_event = Packet::new(
                    opcode::TRACKS_UPDATE,
                    0,
                    serde_json::json!({
                        "action": "remove",
                        "tracks": remove_tracks,
                    }),
                );
                broadcast_to_others(&room, user_id, &tracks_event);
            }

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

        // 5. Room에서 participant 제거 (인덱스 정리 포함)
        if let Err(e) = state.rooms.remove_participant(room_id, user_id) {
            warn!("cleanup error: {e}");
        }

        push_admin_snapshot(state);
        debug!("cleanup done user={} room={}", user_id, room_id);
    }
}
