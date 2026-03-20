// author: kodeholic (powered by Claude)
//! Background tasks — floor timer + zombie reaper
//!
//! lib.rs에서 분리. 주기적 비동기 태스크들을 모아둠.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::config;
use crate::config::RoomMode;
use crate::metrics::GlobalMetrics;
use crate::room::floor::FloorAction;
use crate::room::room::RoomHub;
use crate::signaling::handler::helpers::{broadcast_to_room, build_remove_tracks};
use crate::signaling::message::Packet;
use crate::signaling::opcode;

/// Floor Control 타이머 태스크 (PTT T2/T_FLOOR_TIMEOUT 감시)
/// 2초 주기로 PTT 모드 room의 floor 타이머를 체크하여
/// max burst 초과 또는 ping 미수신 시 발화권을 강제 회수한다.
pub(crate) async fn run_floor_timer(
    rooms: Arc<RoomHub>,
    metrics: Arc<GlobalMetrics>,
    cancel: CancellationToken,
) {
    let mut timer = tokio::time::interval(
        tokio::time::Duration::from_millis(config::FLOOR_PING_INTERVAL_MS),
    );
    timer.tick().await; // 첫 tick 즉시 소비

    loop {
        tokio::select! {
            _ = timer.tick() => {}
            _ = cancel.cancelled() => {
                info!("floor timer stopped (shutdown)");
                return;
            }
        }

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        // PTT 모드 room만 순회
        for entry in rooms.rooms.iter() {
            let room = entry.value();
            if room.mode != RoomMode::Ptt {
                continue;
            }

            if let Some(action) = room.floor.check_timers(now) {
                match &action {
                    FloorAction::Revoked { prev_speaker, cause } => {
                        metrics.ptt_floor_revoked.fetch_add(1, Ordering::Relaxed);
                        warn!("[FLOOR] timer revoke: user={} cause={} room={}",
                            prev_speaker, cause, room.id);

                        // PLI burst cancel + rewriter 정리
                        if let Some(p) = room.get_participant(prev_speaker) {
                            p.cancel_pli_burst();
                        }
                        room.audio_rewriter.clear_speaker();
                        room.video_rewriter.clear_speaker();

                        // 발화자에게 FLOOR_REVOKE
                        if let Some(p) = room.get_participant(prev_speaker) {
                            let revoke = Packet::new(
                                opcode::FLOOR_REVOKE,
                                0,
                                serde_json::json!({
                                    "room_id": &room.id,
                                    "cause": cause,
                                }),
                            );
                            let json = serde_json::to_string(&revoke).unwrap_or_default();
                            let _ = p.ws_tx.send(json);
                        }

                        // 전체에 FLOOR_IDLE
                        let idle = Packet::new(
                            opcode::FLOOR_IDLE,
                            0,
                            serde_json::json!({
                                "room_id": &room.id,
                                "prev_speaker": prev_speaker,
                            }),
                        );
                        broadcast_to_room(&room, &idle);
                    }
                    _ => {} // check_timers는 Revoked만 반환
                }
            }
        }
    }
}

/// 좀비 세션 주기적 정리 태스크
/// last_seen + ZOMBIE_TIMEOUT_MS < now 인 participant를 room에서 제거하고
/// 나머지 참가자들에게 leave/tracks_update 브로드캐스트
pub(crate) async fn run_zombie_reaper(
    rooms: Arc<RoomHub>,
    cancel: CancellationToken,
) {
    let mut timer = tokio::time::interval(
        tokio::time::Duration::from_millis(config::REAPER_INTERVAL_MS),
    );
    timer.tick().await; // 첫 tick 즉시 소비

    loop {
        tokio::select! {
            _ = timer.tick() => {}
            _ = cancel.cancelled() => {
                info!("zombie reaper stopped (shutdown)");
                return;
            }
        }

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        let reaped = rooms.reap_zombies(now, config::ZOMBIE_TIMEOUT_MS);

        for (room_id, zombie) in &reaped {
            // PLI burst cancel (좀비가 발화자였을 수 있음)
            zombie.cancel_pli_burst();

            warn!("zombie reaped: user={} room={} last_seen={}ms ago",
                zombie.user_id, room_id,
                now.saturating_sub(zombie.last_seen.load(Ordering::Relaxed)));

            // 남은 참가자에게 tracks_update(remove) + participant_left 브로드캐스트
            if let Ok(room) = rooms.get(room_id) {
                let tracks = zombie.get_tracks();
                let remove_tracks = build_remove_tracks(&tracks, &zombie.user_id, room.simulcast_enabled, 0);
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

                let leave_event = Packet::new(
                    opcode::ROOM_EVENT,
                    0,
                    serde_json::json!({
                        "type": "participant_left",
                        "room_id": room_id,
                        "user_id": &zombie.user_id,
                    }),
                );
                broadcast_to_room(&room, &leave_event);
            }
        }

        if !reaped.is_empty() {
            debug!("reaper cycle: {} zombies removed", reaped.len());
        }
    }
}
