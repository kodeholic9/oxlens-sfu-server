// author: kodeholic (powered by Claude)
//! Track management handlers — publish, ack, mute, camera, simulcast layer

use std::sync::Arc;
use tracing::{debug, info, warn};
use tokio::time::Duration;

use crate::config::RoomMode;
use crate::room::participant::{TrackKind, SimulcastRewriter, SubscribeLayerEntry};
use crate::signaling::message::*;
use crate::signaling::opcode;
use crate::state::AppState;

use super::Session;
use super::helpers::*;

// ============================================================================
// PUBLISH_TRACKS
// ============================================================================

pub(super) async fn handle_publish_tracks(session: &Session, state: &AppState, packet: &Packet) -> Packet {
    let req: PublishTracksRequest = match serde_json::from_value(packet.d.clone()) {
        Ok(r) => r,
        Err(_) => return Packet::err(opcode::PUBLISH_TRACKS, packet.pid, 3002, "invalid payload"),
    };

    let user_id = session.user_id.as_ref().unwrap();
    let room_id = match &session.current_room {
        Some(r) => r,
        None => return Packet::err(opcode::PUBLISH_TRACKS, packet.pid, 2004, "not in room"),
    };

    let room = match state.rooms.get(room_id) {
        Ok(r) => r,
        Err(_) => return Packet::err(opcode::PUBLISH_TRACKS, packet.pid, 2001, "room not found"),
    };

    let participant = match room.get_participant(user_id) {
        Some(p) => p,
        None => return Packet::err(opcode::PUBLISH_TRACKS, packet.pid, 2004, "not in room"),
    };

    if let Some(twcc_id) = req.twcc_extmap_id {
        participant.twcc_extmap_id.store(twcc_id, std::sync::atomic::Ordering::Relaxed);
    }

    let sim_enabled = room.simulcast_enabled;

    let mut track_id_counter = participant.get_tracks().len();
    let mut new_tracks = Vec::new();
    let mut simulcast_group_counter = 0u32;
    for t in &req.tracks {
        let kind = match t.kind.as_str() {
            "audio" => TrackKind::Audio,
            "video" => TrackKind::Video,
            _ => continue,
        };
        let track_id = format!("{}_{}", user_id, track_id_counter);
        track_id_counter += 1;

        if let Some(ref rid) = t.rid {
            let group = if rid == "h" { simulcast_group_counter } else { simulcast_group_counter };
            if rid == "l" { simulcast_group_counter += 1; }
            participant.add_track_ext(t.ssrc, kind.clone(), track_id.clone(), Some(rid.clone()), Some(group));
        } else {
            participant.add_track(t.ssrc, kind.clone(), track_id.clone());
        }

        let rtx_ssrc = participant.get_tracks().iter()
            .find(|tr| tr.ssrc == t.ssrc)
            .and_then(|tr| tr.rtx_ssrc);

        let mut track_json = serde_json::json!({
            "user_id": user_id,
            "kind": t.kind,
            "ssrc": t.ssrc,
            "track_id": track_id,
        });
        if let Some(rs) = rtx_ssrc {
            track_json["rtx_ssrc"] = serde_json::json!(rs);
        }
        if let Some(ref rid) = t.rid {
            track_json["rid"] = serde_json::json!(rid);
        }
        new_tracks.push(track_json);
    }

    info!("PUBLISH_TRACKS user={} count={} simulcast={}", user_id, new_tracks.len(), sim_enabled);

    if !new_tracks.is_empty() {
        let mut broadcast_tracks: Vec<serde_json::Value> = if sim_enabled {
            new_tracks.iter()
                .filter(|j| j.get("rid").and_then(|v| v.as_str()) != Some("l"))
                .cloned()
                .collect()
        } else {
            new_tracks.clone()
        };
        simulcast_replace_video_ssrc(&mut broadcast_tracks, &room);
        if !broadcast_tracks.is_empty() {
            let tracks_event = Packet::new(
                opcode::TRACKS_UPDATE,
                0,
                serde_json::json!({
                    "action": "add",
                    "tracks": broadcast_tracks,
                }),
            );
            broadcast_to_others(&room, user_id, &tracks_event);
        }
    }

    Packet::ok(opcode::PUBLISH_TRACKS, packet.pid, serde_json::json!({
        "registered": new_tracks.len(),
    }))
}

// ============================================================================
// TRACKS_ACK
// ============================================================================

pub(super) async fn handle_tracks_ack(session: &Session, state: &AppState, packet: &Packet) -> Packet {
    let req: TracksAckRequest = match serde_json::from_value(packet.d.clone()) {
        Ok(r) => r,
        Err(_) => return Packet::err(opcode::TRACKS_ACK, packet.pid, 3002, "invalid payload"),
    };

    let user_id = session.user_id.as_ref().unwrap();
    let room_id = match &session.current_room {
        Some(r) => r,
        None => return Packet::err(opcode::TRACKS_ACK, packet.pid, 2004, "not in room"),
    };

    let room = match state.rooms.get(room_id) {
        Ok(r) => r,
        Err(_) => return Packet::err(opcode::TRACKS_ACK, packet.pid, 2001, "room not found"),
    };

    let expected: std::collections::HashSet<u32> = if room.mode == RoomMode::Ptt {
        let mut set = std::collections::HashSet::new();
        set.insert(room.audio_rewriter.virtual_ssrc());
        set.insert(room.video_rewriter.virtual_ssrc());
        set
    } else {
        let sim = room.simulcast_enabled;
        room.other_participants(user_id)
            .iter()
            .flat_map(|p| {
                let vssrc = if sim { p.ensure_simulcast_video_ssrc() } else { 0 };
                p.get_tracks().into_iter()
                    .filter(move |t| !(sim && t.rid.as_deref() == Some("l")))
                    .map(move |t| {
                        if sim && t.kind == TrackKind::Video { vssrc } else { t.ssrc }
                    })
            })
            .collect()
    };

    let client_set: std::collections::HashSet<u32> = req.ssrcs.into_iter().collect();

    if client_set == expected {
        debug!("TRACKS_ACK ok user={} ssrcs={}", user_id, client_set.len());
        return Packet::ok(opcode::TRACKS_ACK, packet.pid, serde_json::json!({
            "synced": true,
        }));
    }

    state.metrics.tracks_ack_mismatch.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let missing: Vec<u32> = expected.difference(&client_set).copied().collect();
    let extra: Vec<u32> = client_set.difference(&expected).copied().collect();
    warn!("TRACKS_ACK mismatch user={} expected={} client={} missing={:?} extra={:?}",
        user_id, expected.len(), client_set.len(), missing, extra);

    let resync_tracks: Vec<serde_json::Value> = if room.mode == RoomMode::Ptt {
        vec![
            serde_json::json!({ "user_id": "__virtual__", "kind": "audio", "ssrc": room.audio_rewriter.virtual_ssrc(), "track_id": "virtual_audio" }),
            serde_json::json!({ "user_id": "__virtual__", "kind": "video", "ssrc": room.video_rewriter.virtual_ssrc(), "track_id": "virtual_video" }),
        ]
    } else {
        let sim = room.simulcast_enabled;
        let mut resync: Vec<serde_json::Value> = room.other_participants(user_id)
            .iter()
            .flat_map(|p| {
                p.get_tracks().into_iter()
                    .filter(|t| !(sim && t.rid.as_deref() == Some("l")))
                    .map(|t| {
                    let mut j = serde_json::json!({
                        "user_id": p.user_id,
                        "kind": t.kind.to_string(),
                        "ssrc": t.ssrc,
                        "track_id": t.track_id,
                    });
                    if let Some(rs) = t.rtx_ssrc {
                        j["rtx_ssrc"] = serde_json::json!(rs);
                    }
                    j
                })
            })
            .collect();
        simulcast_replace_video_ssrc(&mut resync, &room);
        resync
    };

    let resync = Packet::new(
        opcode::TRACKS_RESYNC,
        0,
        serde_json::json!({ "tracks": resync_tracks }),
    );
    let json = serde_json::to_string(&resync).unwrap_or_default();
    let _ = session.ws_tx.send(json);

    state.metrics.tracks_resync_sent.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    info!("TRACKS_RESYNC sent user={} tracks={}", user_id, resync_tracks.len());

    Packet::ok(opcode::TRACKS_ACK, packet.pid, serde_json::json!({
        "synced": false,
        "resync_sent": true,
    }))
}

// ============================================================================
// MUTE_UPDATE
// ============================================================================

pub(super) async fn handle_mute_update(session: &Session, state: &AppState, packet: &Packet) -> Packet {
    let req: MuteUpdateRequest = match serde_json::from_value(packet.d.clone()) {
        Ok(r) => r,
        Err(_) => return Packet::err(opcode::MUTE_UPDATE, packet.pid, 3002, "invalid payload"),
    };

    let user_id = session.user_id.as_ref().unwrap();
    let room_id = match &session.current_room {
        Some(r) => r,
        None => return Packet::err(opcode::MUTE_UPDATE, packet.pid, 2004, "not in room"),
    };

    let room = match state.rooms.get(room_id) {
        Ok(r) => r,
        Err(_) => return Packet::err(opcode::MUTE_UPDATE, packet.pid, 2001, "room not found"),
    };

    let participant = match room.get_participant(user_id) {
        Some(p) => p,
        None => return Packet::err(opcode::MUTE_UPDATE, packet.pid, 2004, "not in room"),
    };

    let kind = match participant.set_track_muted(req.ssrc, req.muted) {
        Some(k) => k,
        None => return Packet::err(opcode::MUTE_UPDATE, packet.pid, 2005, "track not found"),
    };

    info!("MUTE_UPDATE user={} ssrc={} kind={} muted={}", user_id, req.ssrc, kind, req.muted);

    let state_event = Packet::new(
        opcode::TRACK_STATE,
        0,
        serde_json::json!({ "user_id": user_id, "ssrc": req.ssrc, "kind": kind.to_string(), "muted": req.muted }),
    );
    broadcast_to_others(&room, user_id, &state_event);

    if req.muted && kind == TrackKind::Video {
        let suspended = Packet::new(opcode::VIDEO_SUSPENDED, 0, serde_json::json!({ "user_id": user_id, "room_id": room_id }));
        broadcast_to_others(&room, user_id, &suspended);
        info!("[MUTE] VIDEO_SUSPENDED broadcast user={}", user_id);
    }

    if !req.muted && kind == TrackKind::Video {
        if participant.is_publish_ready() {
            if let Some(pub_addr) = participant.publish.get_address() {
                let pli_plain = crate::transport::udp::build_pli(req.ssrc);
                let encrypted = {
                    let mut ctx = participant.publish.outbound_srtp.lock().unwrap();
                    ctx.encrypt_rtcp(&pli_plain).ok()
                };
                if let Some(enc) = encrypted {
                    let socket = &state.udp_socket;
                    if let Err(e) = socket.send_to(&enc, pub_addr).await {
                        warn!("[MUTE] PLI send FAILED user={} ssrc={}: {e}", user_id, req.ssrc);
                    } else {
                        info!("[MUTE] PLI sent user={} ssrc=0x{:08X} (video unmute)", user_id, req.ssrc);
                    }
                }
            }
        }
    }

    Packet::ok(opcode::MUTE_UPDATE, packet.pid, serde_json::json!({ "ssrc": req.ssrc, "muted": req.muted }))
}

// ============================================================================
// CAMERA_READY
// ============================================================================

pub(super) async fn handle_camera_ready(session: &Session, state: &AppState, packet: &Packet) -> Packet {
    let _req: CameraReadyRequest = match serde_json::from_value(packet.d.clone()) {
        Ok(r) => r,
        Err(_) => return Packet::err(opcode::CAMERA_READY, packet.pid, 3002, "invalid payload"),
    };

    let user_id = session.user_id.as_ref().unwrap();
    let room_id = match &session.current_room {
        Some(r) => r,
        None => return Packet::err(opcode::CAMERA_READY, packet.pid, 2004, "not in room"),
    };
    let room = match state.rooms.get(room_id) {
        Ok(r) => r,
        Err(_) => return Packet::err(opcode::CAMERA_READY, packet.pid, 2001, "room not found"),
    };
    let participant = match room.get_participant(user_id) {
        Some(p) => p,
        None => return Packet::err(opcode::CAMERA_READY, packet.pid, 2004, "not in room"),
    };

    let video_ssrc = {
        let tracks = participant.tracks.lock().unwrap();
        tracks.iter().find(|t| t.kind == TrackKind::Video).map(|t| t.ssrc)
    };
    let ssrc = match video_ssrc {
        Some(s) => s,
        None => {
            info!("[CAMERA_READY] user={} no video track", user_id);
            return Packet::ok(opcode::CAMERA_READY, packet.pid, serde_json::json!({}));
        }
    };

    if participant.is_publish_ready() {
        if let Some(pub_addr) = participant.publish.get_address() {
            participant.cancel_pli_burst();
            let p = Arc::clone(&participant);
            let socket = state.udp_socket.clone();
            let uid = user_id.to_string();
            let handle = tokio::spawn(async move {
                let delays = [0u64, 150];
                for (i, &delay_ms) in delays.iter().enumerate() {
                    if delay_ms > 0 { tokio::time::sleep(Duration::from_millis(delay_ms)).await; }
                    let pli_plain = crate::transport::udp::build_pli(ssrc);
                    let encrypted = {
                        let mut ctx = p.publish.outbound_srtp.lock().unwrap();
                        ctx.encrypt_rtcp(&pli_plain).ok()
                    };
                    if let Some(enc) = encrypted {
                        if let Err(e) = socket.send_to(&enc, pub_addr).await {
                            warn!("[CAMERA_READY] PLI send FAILED user={} ssrc=0x{:08X} #{}: {e}", uid, ssrc, i);
                            break;
                        } else {
                            info!("[CAMERA_READY] PLI sent user={} ssrc=0x{:08X} #{}", uid, ssrc, i);
                        }
                    }
                }
            });
            *participant.pli_burst_handle.lock().unwrap() = Some(handle.abort_handle());
        }
    }

    let resumed = Packet::new(opcode::VIDEO_RESUMED, 0, serde_json::json!({ "user_id": user_id, "room_id": room_id }));
    broadcast_to_others(&room, user_id, &resumed);
    info!("[CAMERA_READY] user={} ssrc=0x{:08X} → PLI 2발 + VIDEO_RESUMED", user_id, ssrc);

    Packet::ok(opcode::CAMERA_READY, packet.pid, serde_json::json!({}))
}

// ============================================================================
// SUBSCRIBE_LAYER — Simulcast 레이어 선택 (Phase 3)
// ============================================================================

pub(super) async fn handle_subscribe_layer(session: &Session, state: &AppState, packet: &Packet) -> Packet {
    let req: SubscribeLayerRequest = match serde_json::from_value(packet.d.clone()) {
        Ok(r) => r,
        Err(_) => return Packet::err(opcode::SUBSCRIBE_LAYER, packet.pid, 3002, "invalid payload"),
    };

    let user_id = session.user_id.as_ref().unwrap();
    let room_id = match &session.current_room {
        Some(r) => r,
        None => return Packet::err(opcode::SUBSCRIBE_LAYER, packet.pid, 2004, "not in room"),
    };
    let room = match state.rooms.get(room_id) {
        Ok(r) => r,
        Err(_) => return Packet::err(opcode::SUBSCRIBE_LAYER, packet.pid, 2001, "room not found"),
    };
    if !room.simulcast_enabled {
        return Packet::err(opcode::SUBSCRIBE_LAYER, packet.pid, 2030, "simulcast not enabled");
    }
    let subscriber = match room.get_participant(user_id) {
        Some(p) => p,
        None => return Packet::err(opcode::SUBSCRIBE_LAYER, packet.pid, 2004, "not in room"),
    };

    for target in &req.targets {
        let publisher = match room.get_participant(&target.user_id) {
            Some(p) => p,
            None => continue,
        };
        let vssrc = publisher.ensure_simulcast_video_ssrc();

        let need_pli = {
            let mut layers = subscriber.subscribe_layers.lock().unwrap();
            let (old_rid, old_initialized) = {
                let old = layers.get(&target.user_id);
                (old.map(|e| e.rid.clone()), old.map(|e| e.rewriter.initialized).unwrap_or(false))
            };
            let entry = layers.entry(target.user_id.clone())
                .or_insert_with(|| SubscribeLayerEntry {
                    rid: target.rid.clone(),
                    rewriter: SimulcastRewriter::new(vssrc),
                });
            if old_rid.as_deref() == Some(&target.rid) {
                !old_initialized && target.rid != "pause"
            } else {
                entry.rid = target.rid.clone();
                if target.rid != "pause" { entry.rewriter.switch_layer(); true } else { false }
            }
        };

        if need_pli {
            let video_ssrc = {
                let tracks = publisher.tracks.lock().unwrap();
                tracks.iter()
                    .find(|t| t.kind == TrackKind::Video && t.rid.as_deref() == Some(&target.rid))
                    .map(|t| t.ssrc)
            };
            if let (Some(ssrc), Some(pub_addr)) = (video_ssrc, publisher.publish.get_address()) {
                if publisher.is_publish_ready() {
                    publisher.cancel_pli_burst();
                    let p = Arc::clone(&publisher);
                    let socket = state.udp_socket.clone();
                    let uid = target.user_id.clone();
                    let rid = target.rid.clone();
                    let handle = tokio::spawn(async move {
                        let delays = [0u64, 200, 500, 1500];
                        for (i, &delay_ms) in delays.iter().enumerate() {
                            if delay_ms > 0 { tokio::time::sleep(Duration::from_millis(delay_ms)).await; }
                            let pli_plain = crate::transport::udp::build_pli(ssrc);
                            let encrypted = {
                                let mut ctx = p.publish.outbound_srtp.lock().unwrap();
                                ctx.encrypt_rtcp(&pli_plain).ok()
                            };
                            if let Some(enc) = encrypted {
                                if let Err(e) = socket.send_to(&enc, pub_addr).await {
                                    warn!("[SIM:PLI] send FAILED user={} rid={} ssrc=0x{:08X} #{}: {e}", uid, rid, ssrc, i);
                                    break;
                                } else {
                                    info!("[SIM:PLI] sent user={} rid={} ssrc=0x{:08X} #{}", uid, rid, ssrc, i);
                                }
                            }
                        }
                    });
                    *publisher.pli_burst_handle.lock().unwrap() = Some(handle.abort_handle());
                }
            }
        }

        info!("SUBSCRIBE_LAYER subscriber={} publisher={} rid={} need_pli={}", user_id, target.user_id, target.rid, need_pli);
    }

    Packet::ok(opcode::SUBSCRIBE_LAYER, packet.pid, serde_json::json!({}))
}
