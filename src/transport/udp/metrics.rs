// author: kodeholic (powered by Claude)
//! Server metrics — B구간 계측 (timing accumulators + RTCP counters)
//! + Tokio RuntimeMetrics + 환경 메타데이터 + Egress encrypt timing

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// 3초 주기 집계를 위한 타이밍 어퀴뮬레이터
pub(crate) struct TimingStat {
    pub(crate) sum_us: u64,
    pub(crate) count:  u64,
    pub(crate) min_us: u64,
    pub(crate) max_us: u64,
}

impl TimingStat {
    pub(crate) fn new() -> Self {
        Self { sum_us: 0, count: 0, min_us: u64::MAX, max_us: 0 }
    }

    pub(crate) fn record(&mut self, us: u64) {
        self.sum_us += us;
        self.count += 1;
        if us < self.min_us { self.min_us = us; }
        if us > self.max_us { self.max_us = us; }
    }

    pub(crate) fn avg(&self) -> u64 {
        if self.count == 0 { 0 } else { self.sum_us / self.count }
    }

    #[allow(dead_code)]
    pub(crate) fn p95_approx(&self) -> u64 {
        // 정확한 p95는 histogram 필요. 단순 근사: max * 0.95 또는 avg + (max-avg)*0.8
        // 여기서는 max를 그대로 노출 (p95 대신 max 사용)
        self.max_us
    }

    pub(crate) fn to_json(&self) -> serde_json::Value {
        if self.count == 0 {
            return serde_json::json!(null);
        }
        serde_json::json!({
            "avg_us": self.avg(),
            "min_us": if self.min_us == u64::MAX { 0 } else { self.min_us },
            "max_us": self.max_us,
            "count":  self.count,
        })
    }

    pub(crate) fn reset(&mut self) {
        self.sum_us = 0;
        self.count = 0;
        self.min_us = u64::MAX;
        self.max_us = 0;
    }
}

pub(crate) struct ServerMetrics {
    // B-1: relay total (decrypt ~ last send_to)
    pub(crate) relay:          TimingStat,
    // B-2: SRTP decrypt
    pub(crate) decrypt:        TimingStat,
    // B-3: SRTP encrypt (per target)
    pub(crate) encrypt:        TimingStat,
    // B-4: Mutex lock wait
    pub(crate) lock_wait:      TimingStat,
    // B-5: fan-out count per relay
    pub(crate) fan_out_sum:    u64,
    pub(crate) fan_out_count:  u64,
    pub(crate) fan_out_min:    u32,
    pub(crate) fan_out_max:    u32,
    // B-6, B-7: encrypt/decrypt failures
    pub(crate) encrypt_fail:   u64,
    pub(crate) decrypt_fail:   u64,
    // B-8~14: RTCP counters
    pub(crate) nack_received:  u64,
    pub(crate) rtx_sent:       u64,
    pub(crate) rtx_cache_miss: u64,
    pub(crate) pli_sent:       u64,
    pub(crate) sr_relayed:     u64,
    pub(crate) rr_relayed:     u64,
    pub(crate) twcc_sent:      u64,
    pub(crate) twcc_recorded:   u64,
    // subscribe RTCP 진단 카운터
    pub(crate) sub_rtcp_received: u64,
    pub(crate) sub_rtcp_not_rtcp: u64,
    pub(crate) sub_rtcp_decrypted: u64,
    // RTX 진단 카운터 (v0.3.9 hotfix)
    pub(crate) rtp_cache_stored: u64,
    pub(crate) nack_publisher_not_found: u64,
    pub(crate) nack_no_rtx_ssrc: u64,
    pub(crate) rtp_cache_lock_fail: u64,
    // Egress 큐 포화 드롭 (v0.3.10)
    pub(crate) egress_drop: u64,
}

impl ServerMetrics {
    pub(crate) fn new() -> Self {
        Self {
            relay:          TimingStat::new(),
            decrypt:        TimingStat::new(),
            encrypt:        TimingStat::new(),
            lock_wait:      TimingStat::new(),
            fan_out_sum:    0,
            fan_out_count:  0,
            fan_out_min:    u32::MAX,
            fan_out_max:    0,
            encrypt_fail:   0,
            decrypt_fail:   0,
            nack_received:  0,
            rtx_sent:       0,
            rtx_cache_miss: 0,
            pli_sent:       0,
            sr_relayed:     0,
            rr_relayed:     0,
            twcc_sent:      0,
            twcc_recorded:   0,
            sub_rtcp_received: 0,
            sub_rtcp_not_rtcp: 0,
            sub_rtcp_decrypted: 0,
            rtp_cache_stored: 0,
            nack_publisher_not_found: 0,
            nack_no_rtx_ssrc: 0,
            rtp_cache_lock_fail: 0,
            egress_drop: 0,
        }
    }

    #[allow(dead_code)]
    pub(crate) fn record_fan_out(&mut self, count: u32) {
        self.fan_out_sum += count as u64;
        self.fan_out_count += 1;
        if count < self.fan_out_min { self.fan_out_min = count; }
        if count > self.fan_out_max { self.fan_out_max = count; }
    }

    pub(crate) fn to_json(&self) -> serde_json::Value {
        let fan_out_avg = if self.fan_out_count == 0 { 0.0 }
            else { self.fan_out_sum as f64 / self.fan_out_count as f64 };
        serde_json::json!({
            "type": "server_metrics",
            "relay":          self.relay.to_json(),
            "decrypt":        self.decrypt.to_json(),
            "encrypt":        self.encrypt.to_json(),
            "lock_wait":      self.lock_wait.to_json(),
            "fan_out": {
                "avg": format!("{:.1}", fan_out_avg),
                "min": if self.fan_out_min == u32::MAX { 0 } else { self.fan_out_min },
                "max": self.fan_out_max,
            },
            "encrypt_fail":   self.encrypt_fail,
            "decrypt_fail":   self.decrypt_fail,
            "nack_received":  self.nack_received,
            "rtx_sent":       self.rtx_sent,
            "rtx_cache_miss": self.rtx_cache_miss,
            "pli_sent":       self.pli_sent,
            "sr_relayed":     self.sr_relayed,
            "rr_relayed":     self.rr_relayed,
            "twcc_sent":      self.twcc_sent,
            "twcc_recorded":   self.twcc_recorded,
            "sub_rtcp_received": self.sub_rtcp_received,
            "sub_rtcp_not_rtcp": self.sub_rtcp_not_rtcp,
            "sub_rtcp_decrypted": self.sub_rtcp_decrypted,
            "rtp_cache_stored": self.rtp_cache_stored,
            "nack_pub_not_found": self.nack_publisher_not_found,
            "nack_no_rtx": self.nack_no_rtx_ssrc,
            "cache_lock_fail": self.rtp_cache_lock_fail,
            "egress_drop": self.egress_drop,
        })
    }

    pub(crate) fn reset(&mut self) {
        self.relay.reset();
        self.decrypt.reset();
        self.encrypt.reset();
        self.lock_wait.reset();
        self.fan_out_sum = 0;
        self.fan_out_count = 0;
        self.fan_out_min = u32::MAX;
        self.fan_out_max = 0;
        self.encrypt_fail = 0;
        self.decrypt_fail = 0;
        self.nack_received = 0;
        self.rtx_sent = 0;
        self.rtx_cache_miss = 0;
        self.pli_sent = 0;
        self.sr_relayed = 0;
        self.rr_relayed = 0;
        self.twcc_sent = 0;
        self.twcc_recorded = 0;
        self.sub_rtcp_received = 0;
        self.sub_rtcp_not_rtcp = 0;
        self.sub_rtcp_decrypted = 0;
        self.rtp_cache_stored = 0;
        self.nack_publisher_not_found = 0;
        self.nack_no_rtx_ssrc = 0;
        self.rtp_cache_lock_fail = 0;
        self.egress_drop = 0;
    }
}

// ============================================================================
// EnvironmentMeta — 서버 실행 환경 (시작 시 1회 결정, flush마다 JSON에 포함)
// ============================================================================

pub(crate) struct EnvironmentMeta {
    pub(crate) build_mode:    &'static str,  // "release" | "debug"
    pub(crate) log_level:     String,
    pub(crate) worker_count:  usize,
    pub(crate) bwe_mode:      String,
    pub(crate) version:       &'static str,
}

impl EnvironmentMeta {
    pub(crate) fn capture(worker_count: usize, bwe_mode: &crate::config::BweMode) -> Self {
        Self {
            build_mode: if cfg!(debug_assertions) { "debug" } else { "release" },
            log_level: std::env::var("RUST_LOG")
                .or_else(|_| std::env::var("LOG_LEVEL"))
                .unwrap_or_else(|_| "info".to_string()),
            worker_count,
            bwe_mode: bwe_mode.to_string(),
            version: env!("CARGO_PKG_VERSION"),
        }
    }

    pub(crate) fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "build_mode":   self.build_mode,
            "log_level":    self.log_level,
            "worker_count": self.worker_count,
            "bwe_mode":     self.bwe_mode,
            "version":      self.version,
        })
    }
}

// ============================================================================
// EgressTimingAtomics — egress task들이 공유하는 encrypt 타이밍 (lock-free)
// ============================================================================

/// 모든 egress task가 Arc로 공유. 패킷당 fetch_add 3회 (~5-10ns).
/// subscriber별 독립 task이지만 같은 카운터에 쓰므로 cache-line contention 있으나,
/// 30sub xd7 60pps = 1800/s fetch_add xd7 3 = 5400/s (위 54µs/s — 무시 가능)
pub(crate) struct EgressTimingAtomics {
    pub(crate) sum_us: AtomicU64,
    pub(crate) count:  AtomicU64,
    pub(crate) max_us: AtomicU64,
}

impl EgressTimingAtomics {
    pub(crate) fn new() -> Self {
        Self {
            sum_us: AtomicU64::new(0),
            count:  AtomicU64::new(0),
            max_us: AtomicU64::new(0),
        }
    }

    /// egress task에서 호출 — encrypt 소요 시간 기록
    pub(crate) fn record(&self, us: u64) {
        self.sum_us.fetch_add(us, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
        // max: CAS loop (Relaxed — 정확도보다 성능 우선)
        let mut cur = self.max_us.load(Ordering::Relaxed);
        while us > cur {
            match self.max_us.compare_exchange_weak(cur, us, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => break,
                Err(actual) => cur = actual,
            }
        }
    }

    /// flush_metrics에서 3초마다 호출 — swap + JSON 생성
    pub(crate) fn flush_to_json(&self) -> serde_json::Value {
        let sum = self.sum_us.swap(0, Ordering::Relaxed);
        let cnt = self.count.swap(0, Ordering::Relaxed);
        let max = self.max_us.swap(0, Ordering::Relaxed);
        if cnt == 0 {
            return serde_json::json!(null);
        }
        serde_json::json!({
            "avg_us": sum / cnt,
            "max_us": max,
            "count":  cnt,
        })
    }
}

// ============================================================================
// TokioRuntimeSnapshot — 3초마다 delta 계산 (hot path 비용 0)
// ============================================================================

pub(crate) struct TokioRuntimeSnapshot {
    // 이전값 (누적 카운터 delta 계산용)
    prev_busy:          Vec<Duration>,  // per worker
    prev_poll_count:    Vec<u64>,
    prev_steal_count:   Vec<u64>,
    prev_noop_count:    Vec<u64>,
    prev_budget_yield:  u64,
    prev_io_ready:      u64,
    last_sampled:       Instant,
}

impl TokioRuntimeSnapshot {
    pub(crate) fn new() -> Self {
        Self {
            prev_busy:         Vec::new(),
            prev_poll_count:   Vec::new(),
            prev_steal_count:  Vec::new(),
            prev_noop_count:   Vec::new(),
            prev_budget_yield: 0,
            prev_io_ready:     0,
            last_sampled:      Instant::now(),
        }
    }

    /// 3초 플러시 시점에 호출 — Tokio 런타임에서 스냅샷 읽고 delta 계산
    pub(crate) fn sample(&mut self) -> serde_json::Value {
        let handle = tokio::runtime::Handle::current();
        let m = handle.metrics();
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_sampled);
        let elapsed_us = elapsed.as_micros().max(1) as f64;
        let num_workers = m.num_workers();

        // 첫 호출 시 prev 배열 초기화
        if self.prev_busy.is_empty() {
            self.prev_busy.resize(num_workers, Duration::ZERO);
            self.prev_poll_count.resize(num_workers, 0);
            self.prev_steal_count.resize(num_workers, 0);
            self.prev_noop_count.resize(num_workers, 0);
        }

        // === 1등급: 핵심 지표 ===

        // busy_ratio: worker별 busy 비율 합산 (0.0~1.0)
        let mut total_busy_us: u64 = 0;
        let mut workers_json = Vec::new();
        for i in 0..num_workers {
            let busy = m.worker_total_busy_duration(i);
            let delta_busy = busy.saturating_sub(self.prev_busy[i]);
            total_busy_us += delta_busy.as_micros() as u64;

            let poll = m.worker_poll_count(i);
            let delta_poll = poll.saturating_sub(self.prev_poll_count[i]);

            let steal = m.worker_steal_count(i);
            let delta_steal = steal.saturating_sub(self.prev_steal_count[i]);

            let noop = m.worker_noop_count(i);
            let delta_noop = noop.saturating_sub(self.prev_noop_count[i]);

            workers_json.push(serde_json::json!({
                "busy_ratio": format!("{:.3}", delta_busy.as_micros() as f64 / elapsed_us),
                "polls": delta_poll,
                "steals": delta_steal,
                "noops": delta_noop,
            }));

            self.prev_busy[i] = busy;
            self.prev_poll_count[i] = poll;
            self.prev_steal_count[i] = steal;
            self.prev_noop_count[i] = noop;
        }

        let overall_busy_ratio = total_busy_us as f64 / (elapsed_us * num_workers as f64);

        // budget_forced_yield delta
        let budget_yield = m.budget_forced_yield_count();
        let delta_budget = budget_yield.saturating_sub(self.prev_budget_yield);
        self.prev_budget_yield = budget_yield;

        // io_driver_ready_count delta
        let io_ready = m.io_driver_ready_count();
        let delta_io = io_ready.saturating_sub(self.prev_io_ready);
        self.prev_io_ready = io_ready;

        // === gauge 지표 ===
        let alive_tasks = m.num_alive_tasks();
        let global_queue = m.global_queue_depth();
        let blocking_threads = m.num_blocking_threads();

        self.last_sampled = now;

        serde_json::json!({
            "busy_ratio":       format!("{:.3}", overall_busy_ratio),
            "alive_tasks":      alive_tasks,
            "global_queue":     global_queue,
            "blocking_threads": blocking_threads,
            "budget_yield":     delta_budget,
            "io_ready":         delta_io,
            "workers":          workers_json,
        })
    }
}
