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

use axum::{
    extract::{State, WebSocketUpgrade, ws::{Message, WebSocket}},
    response::IntoResponse,
};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::time::{Duration, Instant, interval_at};
use tracing::{debug, info, trace, warn};

use crate::config;
use crate::config::RoomMode;
use crate::room::participant::{EgressPacket, Participant, TrackKind};
use crate::room::floor::FloorAction;
use crate::signaling::message::*;
use crate::signaling::opcode;
use crate::state::AppState;
use crate::transport::ice::IceCredentials;

// ============================================================================
// Session (per-WS connection)
// ============================================================================

struct Session {
    user_id:      Option<String>,
    current_room: Option<String>,
    /// publish ufrag (cleanup key)
    pub_ufrag:    Option<String>,
    /// subscribe ufrag (cleanup key)
    sub_ufrag:    Option<String>,
    server_pid:   AtomicU64,
    ack_miss:     AtomicU64,
    ws_tx:        mpsc::UnboundedSender<String>,
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
        opcode::IDENTIFY        => Some(handle_identify(session, state, &packet)),
        opcode::ROOM_LIST       => Some(handle_room_list(state, &packet)),
        opcode::ROOM_CREATE     => Some(handle_room_create(state, &packet)),
        opcode::ROOM_JOIN       => Some(handle_room_join(session, state, &packet).await),
        opcode::ROOM_LEAVE      => Some(handle_room_leave(session, state, &packet).await),
        opcode::PUBLISH_TRACKS  => Some(handle_publish_tracks(session, state, &packet).await),
        opcode::MUTE_UPDATE     => Some(handle_mute_update(session, state, &packet).await),
        opcode::CAMERA_READY    => Some(handle_camera_ready(session, state, &packet).await),
        opcode::MESSAGE         => Some(handle_message(session, state, &packet).await),
        opcode::TELEMETRY      => { handle_telemetry(session, state, &packet); None }
        // Floor Control (MCPTT/MBCP)
        opcode::FLOOR_REQUEST   => Some(handle_floor_request(session, state, &packet).await),
        opcode::FLOOR_RELEASE   => Some(handle_floor_release(session, state, &packet).await),
        opcode::FLOOR_PING      => Some(handle_floor_ping(session, state, &packet)),
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

fn handle_room_create(state: &AppState, packet: &Packet) -> Packet {
    let req: RoomCreateRequest = match serde_json::from_value(packet.d.clone()) {
        Ok(r) => r,
        Err(_) => return Packet::err(opcode::ROOM_CREATE, packet.pid, 3002, "invalid payload"),
    };

    let now = current_ts();
    let mode = req.mode.unwrap_or(RoomModeField::Conference).to_config();
    let room = state.rooms.create(req.name.clone(), req.capacity, mode, now);
    info!("ROOM_CREATE id={} name={} mode={}", room.id, room.name, room.mode);

    Packet::ok(opcode::ROOM_CREATE, packet.pid, serde_json::json!({
        "room_id": room.id,
        "name": room.name,
        "capacity": room.capacity,
        "mode": room.mode.to_string(),
    }))
}

// ============================================================================
// ROOM_JOIN — server_config 응답 (SDP-free)
// ============================================================================

async fn handle_room_join(session: &mut Session, state: &AppState, packet: &Packet) -> Packet {
    let req: RoomJoinRequest = match serde_json::from_value(packet.d.clone()) {
        Ok(r) => r,
        Err(_) => return Packet::err(opcode::ROOM_JOIN, packet.pid, 3002, "invalid payload"),
    };

    let user_id = session.user_id.clone().unwrap();

    // Get room
    let room = match state.rooms.get(&req.room_id) {
        Ok(r) => r,
        Err(_) => return Packet::err(opcode::ROOM_JOIN, packet.pid, 2001, "room not found"),
    };

    // Generate ICE credentials (2 sets: publish + subscribe)
    let pub_ice = IceCredentials::new();
    let sub_ice = IceCredentials::new();
    let now = current_ts();

    // Create Participant with 2PC sessions
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

    // Register in RoomHub (participants + 2 ufrag indices)
    if let Err(e) = state.rooms.add_participant(&req.room_id, Arc::clone(&participant)) {
        return Packet::err(opcode::ROOM_JOIN, packet.pid, e.code(), &e.to_string());
    }

    session.current_room = Some(req.room_id.clone());
    session.pub_ufrag = Some(pub_ice.ufrag.clone());
    session.sub_ufrag = Some(sub_ice.ufrag.clone());

    info!("ROOM_JOIN user={} room={} pub_ufrag={} sub_ufrag={}",
        user_id, req.room_id, pub_ice.ufrag, sub_ice.ufrag);

    // Collect existing participants' tracks for the new joiner (rtx_ssrc 포함)
    let others = room.other_participants(&user_id);
    let existing_tracks: Vec<serde_json::Value> = others
        .iter()
        .flat_map(|p| {
            p.get_tracks().into_iter().map(|t| {
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

    // P1: existing_tracks 상세 로그 — subscribe 누락 추적용
    let per_user: Vec<String> = others
        .iter()
        .map(|p| format!("{}({})", p.user_id, p.get_tracks().len()))
        .collect();
    info!("ROOM_JOIN user={} room={} existing_tracks={} from=[{}]",
        user_id, req.room_id, existing_tracks.len(), per_user.join(", "));

    // Notify existing participants
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

    // Admin snapshot push (room 변경 시)
    push_admin_snapshot(state);

    let members: Vec<String> = room.member_ids();

    // Build server_config response (SDP-free!)
    // IP/port는 AppState에서 (.env PUBLIC_IP / UDP_PORT fallback)
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
            "extmap": server_extmap_policy(state.bwe_mode),
            "max_bitrate_bps": config::resolve_remb_bitrate(),
        },
        "tracks": existing_tracks,
    });

    // Phase E-5: PTT 모드 — 가상 SSRC + 현재 floor 상태
    if room.mode == RoomMode::Ptt {
        response["ptt_virtual_ssrc"] = serde_json::json!({
            "audio": room.audio_rewriter.virtual_ssrc(),
            "video": room.video_rewriter.virtual_ssrc(),
        });
        // 중도 참여자용: 현재 발화자 정보 (없으면 null)
        response["floor_speaker"] = match room.floor.current_speaker() {
            Some(s) => serde_json::json!(s),
            None => serde_json::json!(null),
        };
    }

    Packet::ok(opcode::ROOM_JOIN, packet.pid, response)
}

// ============================================================================
// PUBLISH_TRACKS — 클라이언트가 자기 트랙 SSRC 등록
// ============================================================================

async fn handle_publish_tracks(session: &Session, state: &AppState, packet: &Packet) -> Packet {
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

    // Register tracks on participant
    let mut track_id_counter = participant.get_tracks().len();
    let mut new_tracks = Vec::new();
    for t in &req.tracks {
        let kind = match t.kind.as_str() {
            "audio" => TrackKind::Audio,
            "video" => TrackKind::Video,
            _ => continue,
        };
        let track_id = format!("{}_{}", user_id, track_id_counter);
        track_id_counter += 1;
        participant.add_track(t.ssrc, kind.clone(), track_id.clone());

        // add_track 후 등록된 트랙에서 rtx_ssrc 가져오기
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
        new_tracks.push(track_json);
    }

    info!("PUBLISH_TRACKS user={} count={}", user_id, new_tracks.len());

    // Broadcast tracks_update to other participants
    if !new_tracks.is_empty() {
        let tracks_event = Packet::new(
            opcode::TRACKS_UPDATE,
            0,
            serde_json::json!({
                "action": "add",
                "tracks": new_tracks,
            }),
        );
        broadcast_to_others(&room, user_id, &tracks_event);
    }

    Packet::ok(opcode::PUBLISH_TRACKS, packet.pid, serde_json::json!({
        "registered": new_tracks.len(),
    }))
}

// ============================================================================
// MUTE_UPDATE — 트랙 mute/unmute 상태 변경 + 브로드캐스트
// ============================================================================

async fn handle_mute_update(session: &Session, state: &AppState, packet: &Packet) -> Packet {
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

    // 트랙 mute 상태 갱신
    let kind = match participant.set_track_muted(req.ssrc, req.muted) {
        Some(k) => k,
        None => return Packet::err(opcode::MUTE_UPDATE, packet.pid, 2005, "track not found"),
    };

    info!("MUTE_UPDATE user={} ssrc={} kind={} muted={}",
        user_id, req.ssrc, kind, req.muted);

    // 다른 참가자에게 TRACK_STATE 브로드캐스트
    let state_event = Packet::new(
        opcode::TRACK_STATE,
        0,
        serde_json::json!({
            "user_id": user_id,
            "ssrc": req.ssrc,
            "kind": kind.to_string(),
            "muted": req.muted,
        }),
    );
    broadcast_to_others(&room, user_id, &state_event);

    // Video mute → VIDEO_SUSPENDED 브로드캐스트 (카메라 off → 상대방 UI avatar 전환)
    if req.muted && kind == TrackKind::Video {
        let suspended = Packet::new(
            opcode::VIDEO_SUSPENDED,
            0,
            serde_json::json!({
                "user_id": user_id,
                "room_id": room_id,
            }),
        );
        broadcast_to_others(&room, user_id, &suspended);
        info!("[MUTE] VIDEO_SUSPENDED broadcast user={}", user_id);
    }

    // Video unmute → PLI 전송 (키프레임 요청)
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
                        info!("[MUTE] PLI sent user={} ssrc=0x{:08X} (video unmute)",
                            user_id, req.ssrc);
                    }
                }
            }
        }
    }

    Packet::ok(opcode::MUTE_UPDATE, packet.pid, serde_json::json!({
        "ssrc": req.ssrc,
        "muted": req.muted,
    }))
}

// ============================================================================
// CAMERA_READY — 카메라 웜업 완료 → PLI 2발 + VIDEO_RESUMED 브로드캐스트
// ============================================================================

async fn handle_camera_ready(session: &Session, state: &AppState, packet: &Packet) -> Packet {
    let req: CameraReadyRequest = match serde_json::from_value(packet.d.clone()) {
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

    // 비디오 SSRC 찾기
    let video_ssrc = {
        let tracks = participant.tracks.lock().unwrap();
        tracks.iter()
            .find(|t| t.kind == TrackKind::Video)
            .map(|t| t.ssrc)
    };

    let ssrc = match video_ssrc {
        Some(s) => s,
        None => {
            info!("[CAMERA_READY] user={} no video track", user_id);
            return Packet::ok(opcode::CAMERA_READY, packet.pid, serde_json::json!({}));
        }
    };

    // PLI 2발: 즉시 1회 + 150ms 후 보험 1회
    if participant.is_publish_ready() {
        if let Some(pub_addr) = participant.publish.get_address() {
            // 이전 PLI burst cancel
            participant.cancel_pli_burst();

            let p = Arc::clone(&participant);
            let socket = state.udp_socket.clone();
            let uid = user_id.to_string();
            let handle = tokio::spawn(async move {
                let delays = [0u64, 150];
                for (i, &delay_ms) in delays.iter().enumerate() {
                    if delay_ms > 0 {
                        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                    }
                    let pli_plain = crate::transport::udp::build_pli(ssrc);
                    let encrypted = {
                        let mut ctx = p.publish.outbound_srtp.lock().unwrap();
                        ctx.encrypt_rtcp(&pli_plain).ok()
                    };
                    if let Some(enc) = encrypted {
                        if let Err(e) = socket.send_to(&enc, pub_addr).await {
                            warn!("[CAMERA_READY] PLI send FAILED user={} ssrc=0x{:08X} #{}: {e}",
                                uid, ssrc, i);
                            break;
                        } else {
                            info!("[CAMERA_READY] PLI sent user={} ssrc=0x{:08X} #{}",
                                uid, ssrc, i);
                        }
                    }
                }
            });

            *participant.pli_burst_handle.lock().unwrap() = Some(handle.abort_handle());
        }
    }

    // VIDEO_RESUMED 브로드캐스트 (상대방 UI 복원)
    let resumed = Packet::new(
        opcode::VIDEO_RESUMED,
        0,
        serde_json::json!({
            "user_id": user_id,
            "room_id": room_id,
        }),
    );
    broadcast_to_others(&room, user_id, &resumed);

    info!("[CAMERA_READY] user={} ssrc=0x{:08X} → PLI 2발 + VIDEO_RESUMED", user_id, ssrc);

    Packet::ok(opcode::CAMERA_READY, packet.pid, serde_json::json!({}))
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

    // PLI burst cancel + Floor release + rewriter 정리
    if let Ok(room) = state.rooms.get(&req.room_id) {
        if let Some(p) = room.get_participant(user_id) {
            p.cancel_pli_burst();
        }
        if room.mode == RoomMode::Ptt {
            if let Some(action) = room.floor.on_participant_leave(user_id) {
                apply_floor_action(opcode::FLOOR_RELEASE, 0, &action, &room, user_id);
                let silence = room.audio_rewriter.clear_speaker();
                room.video_rewriter.clear_speaker();
                // Silence flush → 모든 subscriber egress로 fan-out
                if let Some(frames) = silence {
                    for entry in room.participants.iter() {
                        if entry.value().is_subscribe_ready() {
                            for frame in &frames {
                                let _ = entry.value().egress_tx.try_send(
                                    EgressPacket::Rtp(frame.clone())
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    match state.rooms.remove_participant(&req.room_id, user_id) {
        Ok(p) => {
            info!("ROOM_LEAVE user={} room={}", user_id, req.room_id);

            // Notify remaining: participant_left + tracks_update(remove)
            if let Ok(room) = state.rooms.get(&req.room_id) {
                let tracks = p.get_tracks();
                if !tracks.is_empty() {
                    let remove_tracks: Vec<serde_json::Value> = tracks.iter().map(|t| {
                        let mut j = serde_json::json!({
                            "user_id": user_id,
                            "kind": t.kind.to_string(),
                            "ssrc": t.ssrc,
                            "track_id": t.track_id,
                        });
                        if let Some(rs) = t.rtx_ssrc {
                            j["rtx_ssrc"] = serde_json::json!(rs);
                        }
                        j
                    }).collect();
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

            // Admin snapshot push
            push_admin_snapshot(state);

            Packet::ok(opcode::ROOM_LEAVE, packet.pid, serde_json::json!({}))
        }
        Err(e) => Packet::err(opcode::ROOM_LEAVE, packet.pid, e.code(), &e.to_string()),
    }
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
// Floor Control (MCPTT/MBCP)
// ============================================================================

async fn handle_floor_request(session: &Session, state: &AppState, packet: &Packet) -> Packet {
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

    // PTT Granted → audio rewriter 화자 전환 + PLI 전송
    if let FloorAction::Granted { speaker } = &action {
        state.metrics.ptt_floor_granted.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        state.metrics.ptt_speaker_switches.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        room.audio_rewriter.switch_speaker(speaker);
        room.video_rewriter.switch_speaker(speaker);
        // Phase E-5: PLI 반복 전송 (3회, 500ms 간격)
        // Chrome이 track.muted 상태에서 unmute 후 키프레임을 받아야 디코딩 시작
        // 단발 PLI로는 타이밍이 안 맞을 수 있으므로 반복 전송
        if let Some(participant) = room.get_participant(speaker) {
            if participant.is_publish_ready() {
                let video_ssrc = {
                    let tracks = participant.tracks.lock().unwrap();
                    tracks.iter()
                        .find(|t| t.kind == TrackKind::Video)
                        .map(|t| t.ssrc)
                };
                if let (Some(ssrc), Some(pub_addr)) = (video_ssrc, participant.publish.get_address()) {
                    // 즉시 1회 + 500ms/1000ms 후 추가 2회 = 총 3회
                    // 이전 PLI burst cancel
                    participant.cancel_pli_burst();

                    let p = Arc::clone(&participant);
                    let socket = state.udp_socket.clone();
                    let speaker_id = speaker.to_string();
                    let handle = tokio::spawn(async move {
                        // hard_unmute의 getUserMedia + replaceTrack 완료 시간 확보
                        // 0ms: soft_off 복귀용, 500ms/1500ms: hard_off 복귀용
                        let delays = [0u64, 500, 1500];
                        for (i, &delay_ms) in delays.iter().enumerate() {
                            if delay_ms > 0 {
                                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                            }
                            let pli_plain = crate::transport::udp::build_pli(ssrc);
                            let encrypted = {
                                let mut ctx = p.publish.outbound_srtp.lock().unwrap();
                                ctx.encrypt_rtcp(&pli_plain).ok()
                            };
                            if let Some(enc) = encrypted {
                                if let Err(e) = socket.send_to(&enc, pub_addr).await {
                                    warn!("[FLOOR] PLI send FAILED user={} ssrc=0x{:08X} #{}: {e}",
                                        speaker_id, ssrc, i);
                                    break;
                                } else {
                                    info!("[FLOOR] PLI sent user={} ssrc=0x{:08X} #{} (floor granted)",
                                        speaker_id, ssrc, i);
                                }
                            }
                        }
                    });

                    // AbortHandle 저장
                    *participant.pli_burst_handle.lock().unwrap() = Some(handle.abort_handle());
                }
            }
        }
    }

    response
}

async fn handle_floor_release(session: &Session, state: &AppState, packet: &Packet) -> Packet {
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

    // Release/Released 시 rewriter 정리
    if matches!(&action, FloorAction::Released { .. }) {
        state.metrics.ptt_floor_released.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let silence = room.audio_rewriter.clear_speaker();
        room.video_rewriter.clear_speaker();
        // Silence flush → 모든 subscriber egress로 fan-out
        if let Some(frames) = silence {
            for entry in room.participants.iter() {
                if entry.value().is_subscribe_ready() {
                    for frame in &frames {
                        let _ = entry.value().egress_tx.try_send(
                            EgressPacket::Rtp(frame.clone())
                        );
                    }
                }
            }
        }
    }

    apply_floor_action(opcode::FLOOR_RELEASE, packet.pid, &action, &room, user_id)
}

fn handle_floor_ping(session: &Session, state: &AppState, packet: &Packet) -> Packet {
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

/// FloorAction → 응답 패킷 + 브로드캐스트
fn apply_floor_action(
    op: u16,
    pid: u64,
    action: &FloorAction,
    room: &crate::room::room::Room,
    user_id: &str,
) -> Packet {
    match action {
        FloorAction::Granted { speaker } => {
            // 전체에 Floor Taken 브로드캐스트
            let taken = Packet::new(
                opcode::FLOOR_TAKEN,
                0,
                serde_json::json!({
                    "room_id": room.id,
                    "speaker": speaker,
                }),
            );
            broadcast_to_others(room, user_id, &taken);

            Packet::ok(op, pid, serde_json::json!({
                "granted": true,
                "speaker": speaker,
            }))
        }
        FloorAction::Denied { reason, current_speaker } => {
            Packet::err(op, pid, 2010, &format!("{} (speaker={})", reason, current_speaker))
        }
        FloorAction::Released { prev_speaker } => {
            // 전체에 Floor Idle 브로드캐스트
            let idle = Packet::new(
                opcode::FLOOR_IDLE,
                0,
                serde_json::json!({
                    "room_id": room.id,
                    "prev_speaker": prev_speaker,
                }),
            );
            broadcast_to_room(room, &idle);

            Packet::ok(op, pid, serde_json::json!({}))
        }
        FloorAction::Revoked { prev_speaker, cause } => {
            // 발화자에게 Revoke
            let revoke = Packet::new(
                opcode::FLOOR_REVOKE,
                0,
                serde_json::json!({
                    "room_id": room.id,
                    "cause": cause,
                }),
            );
            // prev_speaker에게 직접 전송
            if let Some(p) = room.get_participant(prev_speaker) {
                let json = serde_json::to_string(&revoke).unwrap_or_default();
                let _ = p.ws_tx.send(json);
            }
            // 전체에 Floor Idle
            let idle = Packet::new(
                opcode::FLOOR_IDLE,
                0,
                serde_json::json!({
                    "room_id": room.id,
                    "prev_speaker": prev_speaker,
                    "cause": cause,
                }),
            );
            broadcast_to_room(room, &idle);

            Packet::ok(op, pid, serde_json::json!({}))
        }
        _ => Packet::ok(op, pid, serde_json::json!({})),
    }
}

// ============================================================================
// Cleanup (WS disconnect)
// ============================================================================

async fn cleanup(session: &Session, state: &AppState) {
    if let (Some(room_id), Some(user_id)) = (&session.current_room, &session.user_id) {
        // PLI burst cancel (진행 중인 PLI burst task가 있으면 중단)
        if let Ok(room) = state.rooms.get(room_id) {
            if let Some(p) = room.get_participant(user_id) {
                p.cancel_pli_burst();
            }
        }

        // Floor: 발화자였으면 자동 release + rewriter 정리 (disconnect 시)
        if let Ok(room) = state.rooms.get(room_id) {
            if room.mode == RoomMode::Ptt {
                if let Some(action) = room.floor.on_participant_leave(user_id) {
                    apply_floor_action(opcode::FLOOR_RELEASE, 0, &action, &room, user_id);
                    let silence = room.audio_rewriter.clear_speaker();
                    room.video_rewriter.clear_speaker();
                    // Silence flush → 모든 subscriber egress로 fan-out
                    if let Some(frames) = silence {
                        for entry in room.participants.iter() {
                            if entry.value().is_subscribe_ready() {
                                for frame in &frames {
                                    let _ = entry.value().egress_tx.try_send(
                                        EgressPacket::Rtp(frame.clone())
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }

        // Get tracks before removing (for tracks_update broadcast)
        let tracks = if let Ok(room) = state.rooms.get(room_id) {
            if let Some(p) = room.get_participant(user_id) {
                p.get_tracks()
            } else {
                vec![]
            }
        } else {
            vec![]
        };

        // Notify others before removing
        if let Ok(room) = state.rooms.get(room_id) {
            // tracks_update(remove)
            if !tracks.is_empty() {
                let remove_tracks: Vec<serde_json::Value> = tracks.iter().map(|t| {
                    let mut j = serde_json::json!({
                        "user_id": user_id,
                        "kind": t.kind.to_string(),
                        "ssrc": t.ssrc,
                        "track_id": t.track_id,
                    });
                    if let Some(rs) = t.rtx_ssrc {
                        j["rtx_ssrc"] = serde_json::json!(rs);
                    }
                    j
                }).collect();
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

        if let Err(e) = state.rooms.remove_participant(room_id, user_id) {
            warn!("cleanup error: {e}");
        }

        // Admin snapshot push
        push_admin_snapshot(state);

        debug!("cleanup done user={} room={}", user_id, room_id);
    }
}

// ============================================================================
// Server codec/extmap policy (fixed, no negotiation)
// ============================================================================

fn server_codec_policy() -> serde_json::Value {
    serde_json::json!([
        {
            "kind": "audio",
            "name": "opus",
            "pt": 111,
            "clockrate": 48000,
            "channels": 2,
            "rtcp_fb": ["nack"],
            "fmtp": "minptime=10;useinbandfec=1"
        },
        {
            "kind": "video",
            "name": "VP8",
            "pt": 96,
            "clockrate": 90000,
            "rtx_pt": 97,
            "rtcp_fb": ["nack", "nack pli", "ccm fir", "goog-remb"]
        }
    ])
}

fn server_extmap_policy(bwe_mode: config::BweMode) -> serde_json::Value {
    // BWE 모드에 따라 transport-wide-cc extmap 포함 여부 결정
    // TWCC: extmap id=6 포함 → Chrome GCC delay gradient 기반 적응적 BWE
    // REMB: extmap id=6 제외 → Chrome REMB 모드, 서버 고정 REMB 힌트
    let mut exts = vec![
        serde_json::json!({ "id": 1, "uri": "urn:ietf:params:rtp-hdrext:sdes:mid" }),
        serde_json::json!({ "id": 4, "uri": "urn:ietf:params:rtp-hdrext:ssrc-audio-level" }),
        serde_json::json!({ "id": 5, "uri": "http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time" }),
    ];
    if bwe_mode == config::BweMode::Twcc {
        exts.push(serde_json::json!({ "id": 6, "uri": "http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01" }));
    }
    serde_json::Value::Array(exts)
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

// ============================================================================
// Utility
// ============================================================================

fn rand_u16() -> u16 {
    let mut buf = [0u8; 2];
    getrandom::fill(&mut buf).expect("getrandom failed");
    u16::from_le_bytes(buf)
}

fn current_ts() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// ============================================================================
// TELEMETRY — 클라이언트 telemetry를 어드민 채널로 passthrough
// ============================================================================

fn handle_telemetry(session: &Session, state: &AppState, packet: &Packet) {
    let user_id = match &session.user_id {
        Some(id) => id.clone(),
        None => return,
    };
    let room_id = match &session.current_room {
        Some(r) => r.clone(),
        None => return,
    };

    // 클라이언트 telemetry에 user_id, room_id를 래핑하여 어드민으로 전달
    let admin_msg = serde_json::json!({
        "type": "client_telemetry",
        "user_id": user_id,
        "room_id": room_id,
        "data": packet.d,
    });

    // broadcast::send — receiver 없으면 에러지만 무시
    let _ = state.admin_tx.send(admin_msg.to_string());
    trace!("telemetry forwarded user={} room={}", user_id, room_id);
}

// ============================================================================
// Admin WebSocket handler — telemetry 수신 전용
// ============================================================================

pub async fn admin_ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_admin_connection(socket, state))
}

async fn handle_admin_connection(mut socket: WebSocket, state: AppState) {
    info!("admin WS connected");
    let mut rx = state.admin_tx.subscribe();

    // 접속 즉시 room 상태 스냅샷 전송
    let snapshot = build_rooms_snapshot(&state);
    if let Ok(json) = serde_json::to_string(&snapshot) {
        if socket.send(Message::Text(json.into())).await.is_err() {
            return;
        }
    }

    loop {
        tokio::select! {
            msg = rx.recv() => {
                match msg {
                    Ok(json) => {
                        if socket.send(Message::Text(json.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        warn!("admin WS lagged {} messages", n);
                    }
                    Err(_) => break,
                }
            }
            ws_msg = socket.recv() => {
                match ws_msg {
                    Some(Ok(Message::Close(_))) | None => break,
                    _ => {} // admin은 수신 전용, 클라이언트 메시지 무시
                }
            }
        }
    }

    info!("admin WS disconnected");
}

/// Room 변경 시 admin broadcast 채널로 스냅샷 push
fn push_admin_snapshot(state: &AppState) {
    let snapshot = build_rooms_snapshot(state);
    if let Ok(json) = serde_json::to_string(&snapshot) {
        let _ = state.admin_tx.send(json);
    }
}

/// 어드민 접속 시 전송하는 room 전체 스냅샷
fn build_rooms_snapshot(state: &AppState) -> serde_json::Value {
    let rooms: Vec<serde_json::Value> = state.rooms.rooms
        .iter()
        .map(|entry| {
            let room = entry.value();
            let participants: Vec<serde_json::Value> = room.all_participants()
                .iter()
                .map(|p| {
                    let tracks: Vec<serde_json::Value> = p.get_tracks()
                        .iter()
                        .map(|t| {
                            let mut j = serde_json::json!({
                                "kind": t.kind.to_string(),
                                "ssrc": t.ssrc,
                                "track_id": &t.track_id,
                                "muted": t.muted,
                            });
                            if let Some(rs) = t.rtx_ssrc {
                                j["rtx_ssrc"] = serde_json::json!(rs);
                            }
                            j
                        })
                        .collect();
                    serde_json::json!({
                        "user_id": &p.user_id,
                        "joined_at": p.joined_at,
                        "pub_ready": p.is_publish_ready(),
                        "sub_ready": p.is_subscribe_ready(),
                        "tracks": tracks,
                    })
                })
                .collect();
            let mut room_json = serde_json::json!({
                "room_id": &room.id,
                "name": &room.name,
                "capacity": room.capacity,
                "mode": room.mode.to_string(),
                "created_at": room.created_at,
                "participants": participants,
            });
            // PTT 모드일 때 분석용 상태 정보 추가
            if room.mode == RoomMode::Ptt {
                room_json["ptt"] = serde_json::json!({
                    "floor_speaker": room.floor.current_speaker(),
                    "audio_virtual_ssrc": format!("0x{:08X}", room.audio_rewriter.virtual_ssrc()),
                    "video_virtual_ssrc": format!("0x{:08X}", room.video_rewriter.virtual_ssrc()),
                });
            }
            room_json
        })
        .collect();

    serde_json::json!({
        "type": "snapshot",
        "ts": current_ts(),
        "rooms": rooms,
    })
}
