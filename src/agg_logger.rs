// author: kodeholic (powered by Claude)
//! AggLogger — 반복 이벤트 집계 모듈 (메트릭 연동 전용)
//!
//! 핫패스에서 반복되는 이벤트를 해시키 기반으로 집계.
//! 3초 주기 flush 시 TelemetryBus를 통해 어드민으로 전달.
//!
//! 사용법:
//!   let key = agg_key(&["rtx_filtered", &room_id]);
//!   agg_logger::inc(key);
//!
//!   // 텔레메트리 tick에서
//!   agg_logger::flush();

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use dashmap::DashMap;
use tracing::debug;

/// 키 상한 — 메모리 폭주 방지. 넘으면 inc 무시.
const MAX_KEYS: usize = 1024;

// ============================================================================
// 해시키 생성 헬퍼
// ============================================================================

/// 호출자가 키 재료로 해시키 생성.
pub fn agg_key(parts: &[&str]) -> u64 {
    let mut h = DefaultHasher::new();
    for p in parts { p.hash(&mut h); }
    h.finish()
}

// ============================================================================
// AggEntry — 키당 집계 항목
// ============================================================================

#[derive(Debug)]
struct AggEntry {
    count: AtomicU64,
    /// 이 윈도우 첫 inc 시점 (epoch 기준 nanos)
    first_ns: AtomicU64,
    /// 이 윈도우 마지막 inc 시점 (epoch 기준 nanos)
    last_ns: AtomicU64,
    /// 사람이 읽을 수 있는 라벨
    label: String,
    /// 소속 방 (None = 글로벌 이벤트)
    room_id: Option<String>,
}

/// flush 결과 — 한 윈도우의 집계 레코드
pub struct AggRecord {
    pub key: u64,
    pub label: String,
    pub room_id: Option<String>,
    pub count: u64,
    /// first ~ last 사이 경과 (ms). count=1이면 0.
    pub delta_ms: u64,
}

// ============================================================================
// AggLogger — 전역 싱글톤
// ============================================================================

#[derive(Debug)]
struct AggLogger {
    entries: DashMap<u64, AggEntry>,
    epoch: Instant,
}

static AGG: OnceLock<AggLogger> = OnceLock::new();

/// 서버 시작 시 1회 호출.
pub fn init() {
    AGG.set(AggLogger {
        entries: DashMap::new(),
        epoch: Instant::now(),
    }).expect("AggLogger already initialized");
    debug!("[AGG] initialized");
}

/// 핫패스 진입점 — atomic bump만 (사전 register 필요).
pub fn inc(key: u64) {
    if let Some(agg) = AGG.get() {
        if let Some(entry) = agg.entries.get(&key) {
            entry.count.fetch_add(1, Ordering::Relaxed);
            let now_ns = agg.epoch.elapsed().as_nanos() as u64;
            entry.last_ns.store(now_ns, Ordering::Relaxed);
        }
    }
}

/// 핫패스 진입점 (자동 등록 포함). 미등록 키면 등록 후 카운트.
pub fn inc_with(key: u64, label: String, room_id: Option<&str>) {
    if let Some(agg) = AGG.get() {
        if let Some(entry) = agg.entries.get(&key) {
            entry.count.fetch_add(1, Ordering::Relaxed);
            let now_ns = agg.epoch.elapsed().as_nanos() as u64;
            entry.last_ns.store(now_ns, Ordering::Relaxed);
        } else {
            if agg.entries.len() >= MAX_KEYS { return; }
            let now_ns = agg.epoch.elapsed().as_nanos() as u64;
            agg.entries.insert(key, AggEntry {
                count: AtomicU64::new(1),
                first_ns: AtomicU64::new(now_ns),
                last_ns: AtomicU64::new(now_ns),
                label,
                room_id: room_id.map(|s| s.to_string()),
            });
        }
    }
}

/// 사전 등록 — 핫패스에서 inc(key)만 호출하려면 미리 등록.
pub fn register(key: u64, label: &str, room_id: Option<&str>) {
    if let Some(agg) = AGG.get() {
        if agg.entries.len() >= MAX_KEYS { return; }
        let now_ns = agg.epoch.elapsed().as_nanos() as u64;
        agg.entries.entry(key).or_insert_with(|| AggEntry {
            count: AtomicU64::new(0),
            first_ns: AtomicU64::new(now_ns),
            last_ns: AtomicU64::new(0),
            label: label.to_string(),
            room_id: room_id.map(|s| s.to_string()),
        });
    }
}

/// 활성 세션 종료 시 해당 방의 엔트리 일괄 제거.
pub fn clear_room(room_id: &str) {
    if let Some(agg) = AGG.get() {
        agg.entries.retain(|_, entry| {
            entry.room_id.as_deref() != Some(room_id)
        });
    }
}

/// 3초 주기 flush — drain + TelemetryBus emit.
pub fn flush() {
    let agg = match AGG.get() {
        Some(a) => a,
        None => return,
    };

    let mut records = Vec::new();

    agg.entries.retain(|key, entry| {
        let count = entry.count.swap(0, Ordering::Relaxed);
        if count == 0 {
            return false;
        }
        let first_ns = entry.first_ns.load(Ordering::Relaxed);
        let last_ns = entry.last_ns.load(Ordering::Relaxed);
        let delta_ms = if count <= 1 {
            0
        } else {
            last_ns.saturating_sub(first_ns) / 1_000_000
        };

        records.push(AggRecord {
            key: *key,
            label: entry.label.clone(),
            room_id: entry.room_id.clone(),
            count,
            delta_ms,
        });

        false // drain
    });

    if records.is_empty() { return; }

    let entries_json: Vec<serde_json::Value> = records.iter().map(|r| {
        serde_json::json!({
            "key": format!("{:016x}", r.key),
            "label": r.label,
            "room_id": r.room_id,
            "count": r.count,
            "delta_ms": r.delta_ms,
        })
    }).collect();

    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    let payload = serde_json::json!({
        "type": "agg_log",
        "ts": ts,
        "entries": entries_json,
    });

    crate::telemetry_bus::emit(
        crate::telemetry_bus::TelemetryEvent::AggLog(payload)
    );
}
