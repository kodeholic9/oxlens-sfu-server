// author: kodeholic (powered by Claude)
//! Telemetry handler — 클라이언트 telemetry를 TelemetryBus로 전달

use tracing::trace;

use crate::signaling::message::Packet;
use crate::telemetry_bus::{self, TelemetryEvent};

use super::Session;

pub(super) fn handle_telemetry(session: &Session, packet: &Packet) {
    let user_id = match &session.user_id {
        Some(id) => id.clone(),
        None => return,
    };
    let room_id = match &session.current_room {
        Some(r) => r.clone(),
        None => return,
    };

    telemetry_bus::emit(TelemetryEvent::ClientTelemetry {
        user_id: user_id.clone(),
        room_id: room_id.clone(),
        data: packet.d.clone(),
    });

    trace!("telemetry forwarded user={} room={}", user_id, room_id);
}
