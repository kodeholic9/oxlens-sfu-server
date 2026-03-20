// author: kodeholic (powered by Claude)
//! Telemetry handler — 클라이언트 telemetry를 어드민 채널로 passthrough

use tracing::trace;

use crate::signaling::message::Packet;
use crate::state::AppState;

use super::Session;

pub(super) fn handle_telemetry(session: &Session, state: &AppState, packet: &Packet) {
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
