// author: kodeholic (powered by Claude)
//! GlobalMetrics — Arc 공유, 전체 Atomic, lock-free hot path
//!
//! Phase GM: ServerMetrics + EgressTimingAtomics + spawn atomics 통합.
//! Vuex store처럼 어디서든 &self로 접근 가능한 전역 메트릭 저장소.
//!
//! 구성:
//!   AtomicTimingStat      — lock-free timing accumulator (fetch_add + CAS min/max)
//!   GlobalMetrics         — 전체 메트릭 (Arc로 공유, &self만 필요)
//!   TokioRuntimeSnapshot  — metrics/tokio_snapshot.rs
//!   EnvironmentMeta       — metrics/env.rs

mod env;
mod tokio_snapshot;

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;

use env::EnvironmentMeta;
use tokio_snapshot::TokioRuntimeSnapshot;

// ============================================================================
// AtomicTimingStat — lock-free timing accumulator
// ============================================================================
// EgressTimingAtomics(v0.3.9)에서 검증된 패턴을 일반화.
// fetch_add(sum, count) + CAS loop(min, max). hot path에서 ~10ns.

pub(crate) struct AtomicTimingStat {
    sum_us: AtomicU64,
    count:  AtomicU64,
    min_us: AtomicU64,  // CAS min (init: u64::MAX)
    max_us: AtomicU64,  // CAS max (init: 0)
}

impl AtomicTimingStat {
    pub(crate) fn new() -> Self {
        Self {
            sum_us: AtomicU64::new(0),
            count:  AtomicU64::new(0),
            min_us: AtomicU64::new(u64::MAX),
            max_us: AtomicU64::new(0),
        }
    }

    /// Hot path에서 호출 — &self로 lock-free 기록
    pub(crate) fn record(&self, us: u64) {
        self.sum_us.fetch_add(us, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
        // CAS max
        let mut cur = self.max_us.load(Ordering::Relaxed);
        while us > cur {
            match self.max_us.compare_exchange_weak(cur, us, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => break,
                Err(actual) => cur = actual,
            }
        }
        // CAS min
        let mut cur = self.min_us.load(Ordering::Relaxed);
        while us < cur {
            match self.min_us.compare_exchange_weak(cur, us, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => break,
                Err(actual) => cur = actual,
            }
        }
    }

    /// flush 시 swap(0) → JSON 생성. 3초 윈도우 경계 ±1 오차 무시 가능.
    fn flush_to_json(&self) -> serde_json::Value {
        let sum = self.sum_us.swap(0, Ordering::Relaxed);
        let cnt = self.count.swap(0, Ordering::Relaxed);
        let min = self.min_us.swap(u64::MAX, Ordering::Relaxed);
        let max = self.max_us.swap(0, Ordering::Relaxed);
        if cnt == 0 {
            return serde_json::json!(null);
        }
        serde_json::json!({
            "avg_us": sum / cnt,
            "min_us": if min == u64::MAX { 0 } else { min },
            "max_us": max,
            "count":  cnt,
        })
    }
}

// ============================================================================
// GlobalMetrics — Arc 공유, 전체 Atomic
// ============================================================================

pub(crate) struct GlobalMetrics {
    // ---- Timing accumulators (AtomicTimingStat) ----
    pub(crate) decrypt:         AtomicTimingStat,
    pub(crate) lock_wait:       AtomicTimingStat,
    pub(crate) egress_encrypt:  AtomicTimingStat,

    // ---- Fan-out stats ----
    pub(crate) fan_out_sum:     AtomicU64,
    pub(crate) fan_out_count:   AtomicU64,
    pub(crate) fan_out_min:     AtomicU32,
    pub(crate) fan_out_max:     AtomicU32,

    // ---- Counters (전부 AtomicU64) ----
    pub(crate) encrypt_fail:    AtomicU64,
    pub(crate) decrypt_fail:    AtomicU64,
    pub(crate) nack_received:   AtomicU64,
    pub(crate) nack_seqs_requested: AtomicU64,
    pub(crate) rtx_sent:        AtomicU64,
    pub(crate) rtx_cache_miss:  AtomicU64,
    pub(crate) pli_sent:        AtomicU64,
    pub(crate) sr_relayed:      AtomicU64,
    pub(crate) rr_relayed:      AtomicU64,
    pub(crate) twcc_sent:       AtomicU64,
    pub(crate) twcc_recorded:   AtomicU64,
    pub(crate) remb_sent:       AtomicU64,
    pub(crate) sub_rtcp_received:   AtomicU64,
    pub(crate) sub_rtcp_not_rtcp:   AtomicU64,
    pub(crate) sub_rtcp_decrypted:  AtomicU64,
    pub(crate) rtp_cache_stored:    AtomicU64,
    pub(crate) nack_publisher_not_found: AtomicU64,
    pub(crate) nack_no_rtx_ssrc:    AtomicU64,
    pub(crate) rtp_cache_lock_fail: AtomicU64,
    pub(crate) egress_drop:         AtomicU64,
    // ---- Relay counters (네트워크 vs gate 분리용) ----
    /// publisher → 서버 RTP 수신 성공 (decrypt 성공 기준)
    pub(crate) ingress_rtp_received:  AtomicU64,
    /// 서버 → subscriber RTP relay 성공 (egress 큐 전달 성공)
    pub(crate) egress_rtp_relayed:    AtomicU64,
    /// 서버 → subscriber RTCP relay 성공 (SR relay)
    pub(crate) egress_rtcp_relayed:   AtomicU64,

    // spawn fan-out (W-1 레거시, 현재 미사용이나 JSON 호환 유지)
    pub(crate) spawn_rtp_relayed:   AtomicU64,
    pub(crate) spawn_sr_relayed:    AtomicU64,
    pub(crate) spawn_encrypt_fail:  AtomicU64,

    // ---- PTT (Phase E) ----
    /// E-1 gate에서 드롭된 RTP 수 (비발화자 패킷)
    pub(crate) ptt_rtp_gated:          AtomicU64,
    /// E-2/E-4 리라이팅 성공한 RTP 수
    pub(crate) ptt_rtp_rewritten:      AtomicU64,
    /// E-4 키프레임 대기 중 드롭된 P-frame 수
    pub(crate) ptt_video_pending_drop:  AtomicU64,
    /// E-4 화자 전환 후 키프레임 도착 횟수
    pub(crate) ptt_keyframe_arrived:    AtomicU64,
    /// Floor Granted 횟수
    pub(crate) ptt_floor_granted:       AtomicU64,
    /// Floor Revoked (T2/PING 타임아웃) 횟수
    pub(crate) ptt_floor_revoked:       AtomicU64,
    /// NACK 역매핑 (가상SSRC→원본SSRC) 처리된 NACK 수
    pub(crate) ptt_nack_remapped:       AtomicU64,
    /// Floor Release 횟수
    pub(crate) ptt_floor_released:      AtomicU64,
    /// 화자 전환 횟수 (switch_speaker 호출)
    pub(crate) ptt_speaker_switches:    AtomicU64,
    /// 오디오 리라이팅 성공 횟수
    pub(crate) ptt_audio_rewritten:     AtomicU64,
    /// 비디오 리라이팅 성공 횟수
    pub(crate) ptt_video_rewritten:     AtomicU64,
    /// 비디오 rewriter Skip 횟수 (화자 불일치)
    pub(crate) ptt_video_skip:          AtomicU64,

    // ---- Tokio RuntimeMetrics (flush 시점만, Mutex OK) ----
    tokio_snapshot: Mutex<TokioRuntimeSnapshot>,

    // ---- 환경 메타 (immutable after init) ----
    env_meta: EnvironmentMeta,
}

impl GlobalMetrics {
    pub(crate) fn new(worker_count: usize, bwe_mode: &crate::config::BweMode) -> Self {
        Self {
            decrypt:         AtomicTimingStat::new(),
            lock_wait:       AtomicTimingStat::new(),
            egress_encrypt:  AtomicTimingStat::new(),
            fan_out_sum:     AtomicU64::new(0),
            fan_out_count:   AtomicU64::new(0),
            fan_out_min:     AtomicU32::new(u32::MAX),
            fan_out_max:     AtomicU32::new(0),
            encrypt_fail:    AtomicU64::new(0),
            decrypt_fail:    AtomicU64::new(0),
            nack_received:   AtomicU64::new(0),
            nack_seqs_requested: AtomicU64::new(0),
            rtx_sent:        AtomicU64::new(0),
            rtx_cache_miss:  AtomicU64::new(0),
            pli_sent:        AtomicU64::new(0),
            sr_relayed:      AtomicU64::new(0),
            rr_relayed:      AtomicU64::new(0),
            twcc_sent:       AtomicU64::new(0),
            twcc_recorded:   AtomicU64::new(0),
            remb_sent:       AtomicU64::new(0),
            sub_rtcp_received:   AtomicU64::new(0),
            sub_rtcp_not_rtcp:   AtomicU64::new(0),
            sub_rtcp_decrypted:  AtomicU64::new(0),
            rtp_cache_stored:    AtomicU64::new(0),
            nack_publisher_not_found: AtomicU64::new(0),
            nack_no_rtx_ssrc:    AtomicU64::new(0),
            rtp_cache_lock_fail: AtomicU64::new(0),
            egress_drop:         AtomicU64::new(0),
            ingress_rtp_received:  AtomicU64::new(0),
            egress_rtp_relayed:    AtomicU64::new(0),
            egress_rtcp_relayed:   AtomicU64::new(0),
            spawn_rtp_relayed:   AtomicU64::new(0),
            spawn_sr_relayed:    AtomicU64::new(0),
            spawn_encrypt_fail:  AtomicU64::new(0),
            ptt_rtp_gated:          AtomicU64::new(0),
            ptt_rtp_rewritten:      AtomicU64::new(0),
            ptt_video_pending_drop: AtomicU64::new(0),
            ptt_keyframe_arrived:   AtomicU64::new(0),
            ptt_floor_granted:      AtomicU64::new(0),
            ptt_floor_revoked:      AtomicU64::new(0),
            ptt_nack_remapped:      AtomicU64::new(0),
            ptt_floor_released:     AtomicU64::new(0),
            ptt_speaker_switches:   AtomicU64::new(0),
            ptt_audio_rewritten:    AtomicU64::new(0),
            ptt_video_rewritten:    AtomicU64::new(0),
            ptt_video_skip:         AtomicU64::new(0),
            tokio_snapshot:  Mutex::new(TokioRuntimeSnapshot::new()),
            env_meta:        EnvironmentMeta::capture(worker_count, bwe_mode),
        }
    }

    /// Fan-out 횟수 기록 (CAS min/max)
    #[allow(dead_code)]
    pub(crate) fn record_fan_out(&self, count: u32) {
        self.fan_out_sum.fetch_add(count as u64, Ordering::Relaxed);
        self.fan_out_count.fetch_add(1, Ordering::Relaxed);
        // CAS max
        let mut cur = self.fan_out_max.load(Ordering::Relaxed);
        while count > cur {
            match self.fan_out_max.compare_exchange_weak(cur, count, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => break,
                Err(actual) => cur = actual,
            }
        }
        // CAS min
        let mut cur = self.fan_out_min.load(Ordering::Relaxed);
        while count < cur {
            match self.fan_out_min.compare_exchange_weak(cur, count, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => break,
                Err(actual) => cur = actual,
            }
        }
    }

    /// 3초마다 worker-0에서 호출 — 전체 메트릭 swap(0) → JSON
    pub(crate) fn flush(&self) -> serde_json::Value {
        // Timing stats
        let decrypt_json   = self.decrypt.flush_to_json();
        let lock_wait_json = self.lock_wait.flush_to_json();
        let egress_enc_json = self.egress_encrypt.flush_to_json();

        // Fan-out stats (swap)
        let fo_sum   = self.fan_out_sum.swap(0, Ordering::Relaxed);
        let fo_count = self.fan_out_count.swap(0, Ordering::Relaxed);
        let fo_min   = self.fan_out_min.swap(u32::MAX, Ordering::Relaxed);
        let fo_max   = self.fan_out_max.swap(0, Ordering::Relaxed);
        let fo_avg = if fo_count == 0 { 0.0 } else { fo_sum as f64 / fo_count as f64 };

        // Counters (swap)
        let encrypt_fail   = self.encrypt_fail.swap(0, Ordering::Relaxed);
        let decrypt_fail   = self.decrypt_fail.swap(0, Ordering::Relaxed);
        let nack_received  = self.nack_received.swap(0, Ordering::Relaxed);
        let nack_seqs_requested = self.nack_seqs_requested.swap(0, Ordering::Relaxed);
        let rtx_sent       = self.rtx_sent.swap(0, Ordering::Relaxed);
        let rtx_cache_miss = self.rtx_cache_miss.swap(0, Ordering::Relaxed);
        let pli_sent       = self.pli_sent.swap(0, Ordering::Relaxed);
        let sr_relayed     = self.sr_relayed.swap(0, Ordering::Relaxed);
        let rr_relayed     = self.rr_relayed.swap(0, Ordering::Relaxed);
        let twcc_sent      = self.twcc_sent.swap(0, Ordering::Relaxed);
        let twcc_recorded  = self.twcc_recorded.swap(0, Ordering::Relaxed);
        let remb_sent      = self.remb_sent.swap(0, Ordering::Relaxed);
        let sub_rtcp_received  = self.sub_rtcp_received.swap(0, Ordering::Relaxed);
        let sub_rtcp_not_rtcp  = self.sub_rtcp_not_rtcp.swap(0, Ordering::Relaxed);
        let sub_rtcp_decrypted = self.sub_rtcp_decrypted.swap(0, Ordering::Relaxed);
        let rtp_cache_stored   = self.rtp_cache_stored.swap(0, Ordering::Relaxed);
        let nack_pub_not_found = self.nack_publisher_not_found.swap(0, Ordering::Relaxed);
        let nack_no_rtx        = self.nack_no_rtx_ssrc.swap(0, Ordering::Relaxed);
        let cache_lock_fail    = self.rtp_cache_lock_fail.swap(0, Ordering::Relaxed);
        let egress_drop        = self.egress_drop.swap(0, Ordering::Relaxed);
        let ingress_rtp_received = self.ingress_rtp_received.swap(0, Ordering::Relaxed);
        let egress_rtp_relayed   = self.egress_rtp_relayed.swap(0, Ordering::Relaxed);
        let egress_rtcp_relayed  = self.egress_rtcp_relayed.swap(0, Ordering::Relaxed);
        let spawn_rtp_relayed  = self.spawn_rtp_relayed.swap(0, Ordering::Relaxed);
        let spawn_sr_relayed   = self.spawn_sr_relayed.swap(0, Ordering::Relaxed);
        let spawn_encrypt_fail = self.spawn_encrypt_fail.swap(0, Ordering::Relaxed);
        // PTT counters
        let ptt_rtp_gated         = self.ptt_rtp_gated.swap(0, Ordering::Relaxed);
        let ptt_rtp_rewritten     = self.ptt_rtp_rewritten.swap(0, Ordering::Relaxed);
        let ptt_video_pending     = self.ptt_video_pending_drop.swap(0, Ordering::Relaxed);
        let ptt_keyframe          = self.ptt_keyframe_arrived.swap(0, Ordering::Relaxed);
        let ptt_granted           = self.ptt_floor_granted.swap(0, Ordering::Relaxed);
        let ptt_revoked           = self.ptt_floor_revoked.swap(0, Ordering::Relaxed);
        let ptt_nack_remap        = self.ptt_nack_remapped.swap(0, Ordering::Relaxed);
        let ptt_released          = self.ptt_floor_released.swap(0, Ordering::Relaxed);
        let ptt_switches          = self.ptt_speaker_switches.swap(0, Ordering::Relaxed);
        let ptt_audio_rw          = self.ptt_audio_rewritten.swap(0, Ordering::Relaxed);
        let ptt_video_rw          = self.ptt_video_rewritten.swap(0, Ordering::Relaxed);
        let ptt_video_skip        = self.ptt_video_skip.swap(0, Ordering::Relaxed);

        // Tokio runtime (Mutex, 3초 1회)
        let tokio_json = self.tokio_snapshot.lock().unwrap().sample();

        // 환경 메타 (immutable)
        let env_json = self.env_meta.to_json();

        // json! 매크로 재귀 한계 방지: 하위 객체 분할 조립
        let counters_json = serde_json::json!({
            "encrypt_fail":       encrypt_fail,
            "decrypt_fail":       decrypt_fail,
            "nack_received":      nack_received,
            "nack_seqs_requested": nack_seqs_requested,
            "rtx_sent":           rtx_sent,
            "rtx_cache_miss":     rtx_cache_miss,
            "pli_sent":           pli_sent,
            "sr_relayed":         sr_relayed,
            "rr_relayed":         rr_relayed,
            "twcc_sent":          twcc_sent,
            "twcc_recorded":      twcc_recorded,
            "remb_sent":          remb_sent,
            "sub_rtcp_received":  sub_rtcp_received,
            "sub_rtcp_not_rtcp":  sub_rtcp_not_rtcp,
            "sub_rtcp_decrypted": sub_rtcp_decrypted,
            "rtp_cache_stored":   rtp_cache_stored,
            "nack_pub_not_found": nack_pub_not_found,
            "nack_no_rtx":        nack_no_rtx,
            "cache_lock_fail":    cache_lock_fail,
            "egress_drop":        egress_drop,
            "ingress_rtp_received": ingress_rtp_received,
            "egress_rtp_relayed":   egress_rtp_relayed,
            "egress_rtcp_relayed":  egress_rtcp_relayed,
            "spawn_rtp_relayed":  spawn_rtp_relayed,
            "spawn_sr_relayed":   spawn_sr_relayed,
            "spawn_encrypt_fail": spawn_encrypt_fail,
        });

        let ptt_json = serde_json::json!({
            "rtp_gated":         ptt_rtp_gated,
            "rtp_rewritten":     ptt_rtp_rewritten,
            "audio_rewritten":   ptt_audio_rw,
            "video_rewritten":   ptt_video_rw,
            "video_skip":        ptt_video_skip,
            "video_pending_drop":ptt_video_pending,
            "keyframe_arrived":  ptt_keyframe,
            "floor_granted":     ptt_granted,
            "floor_released":    ptt_released,
            "floor_revoked":     ptt_revoked,
            "speaker_switches":  ptt_switches,
            "nack_remapped":     ptt_nack_remap,
        });

        // 최종 조립: 필드 수를 ~15개로 제한하여 매크로 재귀 안전
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let mut root = serde_json::json!({
            "type": "server_metrics",
            "ts": ts,
            "decrypt":        decrypt_json,
            "lock_wait":      lock_wait_json,
            "egress_encrypt": egress_enc_json,
            "fan_out": {
                "avg": format!("{:.1}", fo_avg),
                "min": if fo_min == u32::MAX { 0 } else { fo_min },
                "max": fo_max,
            },
            "ptt":            ptt_json,
            "env":            env_json,
            "tokio_runtime":  tokio_json,
        });

        // counters를 root에 flat merge
        if let (Some(root_obj), Some(cnt_obj)) = (root.as_object_mut(), counters_json.as_object()) {
            for (k, v) in cnt_obj {
                root_obj.insert(k.clone(), v.clone());
            }
        }

        root
    }
}
