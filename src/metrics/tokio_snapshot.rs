// author: kodeholic (powered by Claude)
//! TokioRuntimeSnapshot — 3초마다 delta 계산 (hot path 비용 0)

use std::time::{Duration, Instant};

pub(crate) struct TokioRuntimeSnapshot {
    // 이전값 (누적 카운터 delta 계산용)
    prev_busy:          Vec<Duration>,
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

        let budget_yield = m.budget_forced_yield_count();
        let delta_budget = budget_yield.saturating_sub(self.prev_budget_yield);
        self.prev_budget_yield = budget_yield;

        let io_ready = m.io_driver_ready_count();
        let delta_io = io_ready.saturating_sub(self.prev_io_ready);
        self.prev_io_ready = io_ready;

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
