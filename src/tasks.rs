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
use crate::room::participant::TrackKind;
use crate::room::room::RoomHub;
use crate::signaling::handler::helpers::{broadcast_to_room, build_remove_tracks};
use crate::signaling::message::Packet;
use crate::signaling::opcode;
use crate::transport::udp::spawn_pli_burst;

/// Floor Control 타이머 태스크 (PTT T2/T_FLOOR_TIMEOUT 감시)
/// 2초 주기로 PTT 모드 room의 floor 타이머를 체크하여
/// max burst 초과 또는 ping 미수신 시 발화권을 강제 회수한다.
/// v2: Revoke 후 큐에서 다음 발화자 자동 grant.
pub(crate) async fn run_floor_timer(
    rooms: Arc<RoomHub>,
    metrics: Arc<GlobalMetrics>,
    udp_socket: Arc<tokio::net::UdpSocket>,
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

            let actions = room.floor.check_timers(now);
            if actions.is_empty() {
                continue;
            }

            for action in &actions {
                match action {
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
                                "cause": cause,
                            }),
                        );
                        broadcast_to_room(&room, &idle);
                    }
                    FloorAction::Granted { speaker, priority, duration_s } => {
                        // 큐 pop에 의한 자동 grant
                        metrics.ptt_floor_granted.fetch_add(1, Ordering::Relaxed);
                        metrics.ptt_speaker_switches.fetch_add(1, Ordering::Relaxed);
                        metrics.ptt_floor_queue_pop.fetch_add(1, Ordering::Relaxed);

                        room.audio_rewriter.switch_speaker(speaker);
                        room.video_rewriter.switch_speaker(speaker);

                        info!("[FLOOR] timer queue pop → granted user={} priority={} room={}",
                            speaker, priority, room.id);

                        // PLI burst for new speaker
                        if let Some(participant) = room.get_participant(speaker) {
                            if participant.is_publish_ready() {
                                if let Some(pub_addr) = participant.publish.get_address() {
                                    let video_ssrc = {
                                        let tracks = participant.tracks.lock().unwrap();
                                        tracks.iter().find(|t| t.kind == TrackKind::Video).map(|t| t.ssrc)
                                    };
                                    if let Some(ssrc) = video_ssrc {
                                        spawn_pli_burst(&participant, ssrc, pub_addr, udp_socket.clone(), &[0, 500, 1500], "FLOOR_QPOP");
                                    }
                                }
                            }
                        }

                        // 해당 사용자에게 Granted 응답
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

                        // 전체에 FLOOR_TAKEN
                        let taken = Packet::new(
                            opcode::FLOOR_TAKEN,
                            0,
                            serde_json::json!({
                                "room_id": &room.id,
                                "speaker": speaker,
                                "priority": priority,
                            }),
                        );
                        broadcast_to_room(&room, &taken);
                    }
                    _ => {} // 다른 액션은 check_timers에서 발생하지 않음
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
