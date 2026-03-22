// author: kodeholic (powered by Claude)
//! Shared helpers for signaling handlers

use tracing::{debug, trace};

use crate::config;
use crate::room::participant::{EgressPacket, Track};
use crate::room::room::Room;
use crate::signaling::message::Packet;
use crate::state::AppState;

// ============================================================================
// Broadcast helpers
// ============================================================================

pub(super) fn broadcast_to_others(room: &crate::room::room::Room, exclude: &str, packet: &Packet) {
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

pub(crate) fn broadcast_to_room(room: &crate::room::room::Room, packet: &Packet) {
    let json = match serde_json::to_string(packet) {
        Ok(j) => j,
        Err(_) => return,
    };
    for entry in room.participants.iter() {
        let _ = entry.value().ws_tx.send(json.clone());
    }
}

// ============================================================================
// Server codec/extmap policy (fixed, no negotiation)
// ============================================================================

pub(super) fn server_codec_policy() -> serde_json::Value {
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
        },
        {
            "kind": "video",
            "name": "H264",
            "pt": 102,
            "clockrate": 90000,
            "rtx_pt": 103,
            "rtcp_fb": ["nack", "nack pli", "ccm fir", "goog-remb"],
            "fmtp": "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
        }
    ])
}

pub(super) fn server_extmap_policy(bwe_mode: config::BweMode, simulcast_enabled: bool) -> serde_json::Value {
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
    // Simulcast: rtp-stream-id + repaired-rtp-stream-id (rid 식별용)
    if simulcast_enabled {
        exts.push(serde_json::json!({ "id": 10, "uri": "urn:ietf:params:rtp-hdrext:sdes:rtp-stream-id" }));
        exts.push(serde_json::json!({ "id": 11, "uri": "urn:ietf:params:rtp-hdrext:sdes:repaired-rtp-stream-id" }));
    }
    serde_json::Value::Array(exts)
}

// ============================================================================
// Simulcast SSRC helpers
// ============================================================================

/// Simulcast 방에서 video 트랙의 SSRC를 가상 SSRC로 교체 + rid 제거
/// subscriber에게 단일 video m-line만 보이도록 함
pub(super) fn simulcast_replace_video_ssrc(tracks: &mut [serde_json::Value], room: &crate::room::room::Room) {
    if !room.simulcast_enabled { return; }
    for t in tracks.iter_mut() {
        if t.get("kind").and_then(|v| v.as_str()) != Some("video") { continue; }
        let user_id = match t.get("user_id").and_then(|v| v.as_str()) {
            Some(uid) => uid.to_string(),
            None => continue,
        };
        if let Some(p) = room.get_participant(&user_id) {
            let vssrc = p.ensure_simulcast_video_ssrc();
            t["ssrc"] = serde_json::json!(vssrc);
            trace!("[SIM] replaced video ssrc → virtual 0x{:08X} for user={}", vssrc, user_id);
        }
        if let Some(obj) = t.as_object_mut() { obj.remove("rid"); }
    }
}

/// ROOM_LEAVE/cleanup용: 이미 제거된 participant의 가상 SSRC로 video 트랙 교체
pub(super) fn simulcast_replace_video_ssrc_direct(tracks: &mut [serde_json::Value], sim_enabled: bool, vssrc: u32) {
    if !sim_enabled || vssrc == 0 { return; }
    for t in tracks.iter_mut() {
        if t.get("kind").and_then(|v| v.as_str()) != Some("video") { continue; }
        t["ssrc"] = serde_json::json!(vssrc);
        if let Some(obj) = t.as_object_mut() { obj.remove("rid"); }
    }
}

// ============================================================================
// Utility
// ============================================================================

pub(super) fn rand_u16() -> u16 {
    let mut buf = [0u8; 2];
    getrandom::fill(&mut buf).expect("getrandom failed");
    u16::from_le_bytes(buf)
}

pub(super) fn current_ts() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Room 변경 시 TelemetryBus로 스냅샷 push
pub(super) fn push_admin_snapshot(state: &AppState) {
    let snapshot = super::admin::build_rooms_snapshot(state);
    crate::telemetry_bus::emit(
        crate::telemetry_bus::TelemetryEvent::RoomSnapshot(snapshot)
    );
}

// ============================================================================
// Subscribe tracks 수집 (Conference/Simulcast 공통)
// ============================================================================

/// 다른 참여자들의 subscribe 트랙 목록을 수집.
/// - Simulcast 방: rid="l" 트랙 제외, video SSRC를 가상 SSRC로 교체
/// - Conference 방: 그대로 수집
///
/// handle_room_join, handle_room_sync, handle_tracks_ack 3곳에서 공용.
pub(super) fn collect_subscribe_tracks(room: &Room, exclude_user: &str) -> Vec<serde_json::Value> {
    let sim_enabled = room.simulcast_enabled;
    let mut tracks: Vec<serde_json::Value> = room.other_participants(exclude_user)
        .iter()
        .flat_map(|p| {
            p.get_tracks().into_iter()
                .filter(|t| !(sim_enabled && t.rid.as_deref() == Some("l")))
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
    simulcast_replace_video_ssrc(&mut tracks, room);
    tracks
}

// ============================================================================
// Remove tracks JSON 빌드 (퇴장/cleanup/reaper 공통)
// ============================================================================

/// 퇴장하는 참여자의 트랙 목록을 TRACKS_UPDATE(remove)용 JSON으로 변환.
/// - Simulcast 방: rid="l" 트랙 제외, video SSRC를 가상 SSRC로 교체
/// - Conference 방: 그대로 변환
///
/// handle_room_leave, cleanup, run_zombie_reaper 3곳에서 공용.
pub(crate) fn build_remove_tracks(
    tracks: &[Track],
    user_id: &str,
    sim_enabled: bool,
    vssrc: u32,
) -> Vec<serde_json::Value> {
    let mut remove_tracks: Vec<serde_json::Value> = tracks.iter()
        .filter(|t| !(sim_enabled && t.rid.as_deref() == Some("l")))
        .map(|t| {
            let mut j = serde_json::json!({
                "user_id": user_id,
                "kind": t.kind.to_string(),
                "ssrc": t.ssrc,
                "track_id": &t.track_id,
            });
            if let Some(rs) = t.rtx_ssrc {
                j["rtx_ssrc"] = serde_json::json!(rs);
            }
            j
        }).collect();
    simulcast_replace_video_ssrc_direct(&mut remove_tracks, sim_enabled, vssrc);
    remove_tracks
}

// ============================================================================
// PTT silence flush (발화권 해제/퇴장 공통)
// ============================================================================

/// PTT 모드에서 발화 종료 시 silence 프레임 생성 + 브로드캐스트.
/// audio_rewriter.clear_speaker() + video_rewriter.clear_speaker() + silence fan-out.
///
/// handle_floor_release, handle_room_leave, cleanup 3곳에서 공용.
/// 퇴장한 유저의 subscribe_layers entry를 다른 참가자에서 제거.
/// 재입장 시 stale rewriter(이전 vSSRC + initialized=true)가 남아있으면
/// PLI 미발송 + SSRC 불일치로 video fan-out 실패.
pub(super) fn purge_subscribe_layers(room: &Room, leaving_user: &str) {
    let mut purged = 0u32;
    for entry in room.participants.iter() {
        if entry.key() == leaving_user { continue; }
        if entry.value().subscribe_layers.lock().unwrap().remove(leaving_user).is_some() {
            purged += 1;
        }
    }
    if purged > 0 {
        debug!("purge_subscribe_layers user={} removed from {} subscribers", leaving_user, purged);
    }
}

pub(super) fn flush_ptt_silence(room: &Room) {
    let silence = room.audio_rewriter.clear_speaker();
    room.video_rewriter.clear_speaker();
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
