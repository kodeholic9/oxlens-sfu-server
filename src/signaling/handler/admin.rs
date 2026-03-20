// author: kodeholic (powered by Claude)
//! Admin WebSocket handler — telemetry 수신 전용 + room snapshot

use axum::{
    extract::{State, WebSocketUpgrade, ws::{Message, WebSocket}},
    response::IntoResponse,
};
use tracing::{info, warn};

use crate::config::RoomMode;
use crate::state::AppState;

use super::helpers::current_ts;

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
    let mut rx = match crate::telemetry_bus::subscribe() {
        Some(rx) => rx,
        None => {
            warn!("admin WS: TelemetryBus not initialized");
            return;
        }
    };

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

// ============================================================================
// Room snapshot builder
// ============================================================================

/// 어드민 접속 시 전송하는 room 전체 스냅샷
pub(super) fn build_rooms_snapshot(state: &AppState) -> serde_json::Value {
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
                "simulcast": room.simulcast_enabled,
                "created_at": room.created_at,
                "participants": participants,
            });
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
