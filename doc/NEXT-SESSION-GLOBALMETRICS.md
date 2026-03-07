# 다음 세션 프롬프트 — GlobalMetrics 리팩터링 (v0.4.0)

## 작업 디렉토리
- 서버: `D:\X.WORK\GitHub\repository\light-livechat`
- 클라이언트/어드민: `D:\X.WORK\GitHub\repository\insight-lens`

## 세션 시작 절차
1. GUIDELINES.md 읽기
2. 아래 설계 확인 후 코딩 진행

## 현재 상태 (v0.3.10)
- TWCC, Telemetry Visibility(환경메타+Egress timing+Tokio RuntimeMetrics), Hot path Vec 제거, egress_drop 카운터까지 완료
- release 2인 테스트: lost 0, jb_delay 72~95ms, busy 1.1%, egress_drop 0 — 안정

## 작업 목표: ServerMetrics → GlobalMetrics (Arc + 전체 AtomicU64)

### 문제
현재 `ServerMetrics`는 `UdpTransport` 내부에 단일 소유(`&mut self` 필요):
- 메트릭 기록을 위해 `handle_srtp` 등 hot path 메서드가 전부 `&mut self`
- egress task에는 별도 `Arc<EgressTimingAtomics>` 파라미터 전달
- spawn fan-out용 `Arc<AtomicU64>` 3개(`spawn_rtp_relayed`, `spawn_sr_relayed`, `spawn_encrypt_fail`) 별도 관리
- 새 카운터 추가 시 `new()`, `to_json()`, `reset()` 3곳 보일러플레이트

### 해결: GlobalMetrics (Arc 공유, 전체 Atomic)
Vue의 Vuex store처럼 어디서든 `&self`로 접근 가능한 전역 메트릭 저장소.

```rust
pub struct GlobalMetrics {
    // TimingStat → AtomicTimingStat (fetch_add + CAS max)
    pub relay:          AtomicTimingStat,
    pub decrypt:        AtomicTimingStat,
    pub encrypt:        AtomicTimingStat,  // 기존 ingress encrypt (현재 미사용)
    pub lock_wait:      AtomicTimingStat,
    pub egress_encrypt: AtomicTimingStat,  // EgressTimingAtomics 흡수
    
    // 카운터 — 전부 AtomicU64
    pub fan_out_sum:    AtomicU64,
    pub fan_out_count:  AtomicU64,
    pub fan_out_min:    AtomicU32,  // CAS min
    pub fan_out_max:    AtomicU32,  // CAS max
    pub encrypt_fail:   AtomicU64,
    pub decrypt_fail:   AtomicU64,
    pub nack_received:  AtomicU64,
    pub rtx_sent:       AtomicU64,
    pub rtx_cache_miss: AtomicU64,
    pub pli_sent:       AtomicU64,
    pub sr_relayed:     AtomicU64,
    pub rr_relayed:     AtomicU64,
    pub twcc_sent:      AtomicU64,
    pub twcc_recorded:  AtomicU64,
    pub sub_rtcp_received:  AtomicU64,
    pub sub_rtcp_not_rtcp:  AtomicU64,
    pub sub_rtcp_decrypted: AtomicU64,
    pub rtp_cache_stored:   AtomicU64,
    pub nack_publisher_not_found: AtomicU64,
    pub nack_no_rtx_ssrc:  AtomicU64,
    pub rtp_cache_lock_fail: AtomicU64,
    pub egress_drop:    AtomicU64,
    
    // Tokio RuntimeMetrics (flush 시점에만 접근, Mutex OK)
    pub tokio_snapshot: Mutex<TokioRuntimeSnapshot>,
    
    // 환경 메타 (immutable after init)
    pub env_meta: EnvironmentMeta,
}
```

### AtomicTimingStat 구현 (EgressTimingAtomics 일반화)
```rust
pub struct AtomicTimingStat {
    sum_us: AtomicU64,
    count:  AtomicU64,
    max_us: AtomicU64,  // CAS loop
}

impl AtomicTimingStat {
    pub fn record(&self, us: u64) { /* fetch_add + CAS max */ }
    pub fn flush_to_json(&self) -> Value { /* swap(0) + JSON */ }
}
```
v0.3.9 EgressTimingAtomics에서 검증 완료된 패턴 그대로.

### 변경 파일 목록

1. `src/transport/udp/metrics.rs`
   - `TimingStat` → 내부 flush 전용으로 유지하거나 제거
   - `AtomicTimingStat` 신규 (EgressTimingAtomics 대체)
   - `GlobalMetrics` 신규 (ServerMetrics + EgressTimingAtomics + spawn atomics 통합)
   - `EgressTimingAtomics` 제거 (GlobalMetrics로 흡수)
   - `TokioRuntimeSnapshot` 유지 (GlobalMetrics 내 Mutex 필드)
   - `EnvironmentMeta` 유지 (GlobalMetrics 내 필드)

2. `src/transport/udp/mod.rs` (UdpTransport)
   - `metrics: ServerMetrics` → `metrics: Arc<GlobalMetrics>` (공유)
   - `egress_timing: Arc<EgressTimingAtomics>` 제거
   - `tokio_snapshot: TokioRuntimeSnapshot` 제거 (GlobalMetrics 내부)
   - `env_meta: EnvironmentMeta` 제거 (GlobalMetrics 내부)
   - `spawn_rtp_relayed/sr_relayed/encrypt_fail: Arc<AtomicU64>` 제거 (GlobalMetrics 내부)
   - `flush_metrics()` → `GlobalMetrics::flush()` 호출
   - 생성자들 정리

3. `src/transport/udp/ingress.rs`
   - `&mut self` → `&self` 복귀 (metrics가 Arc<GlobalMetrics>이므로)
   - `self.metrics.decrypt.record()` → `self.metrics.decrypt.record()` (동일 API, 내부만 atomic)
   - `self.metrics.decrypt_fail += 1` → `self.metrics.decrypt_fail.fetch_add(1, Relaxed)`

4. `src/transport/udp/egress.rs`
   - `run_egress_task` 시그니처에서 `timing: Arc<EgressTimingAtomics>` 제거
   - 대신 `metrics: Arc<GlobalMetrics>` 전달 (또는 participant에서 접근)
   - `timing.record()` → `metrics.egress_encrypt.record()`

5. `src/lib.rs`
   - `Arc<GlobalMetrics>` 생성 → UdpTransport들에 공유
   - spawn fan-out atomic 3개 제거

6. `src/state.rs` (선택)
   - AppState에 `Arc<GlobalMetrics>` 추가 가능 (handler에서 접근 필요 시)

### flush 패턴
```rust
impl GlobalMetrics {
    /// 3초마다 worker-0에서 호출
    pub fn flush(&self) -> serde_json::Value {
        let mut json = serde_json::json!({
            "type": "server_metrics",
            "relay": self.relay.flush_to_json(),
            "decrypt": self.decrypt.flush_to_json(),
            // ...
            "egress_encrypt": self.egress_encrypt.flush_to_json(),
            "env": self.env_meta.to_json(),
            "tokio_runtime": self.tokio_snapshot.lock().unwrap().sample(),
        });
        // 카운터: swap(0)
        // ...
        json
    }
}
```

### 주의사항
- `flush()` 시 `swap(0, Relaxed)` — 3초 윈도우 경계에서 1-2 패킷 누락 가능하나 무시 가능
- `AtomicTimingStat.flush_to_json()`에서 sum/count/max를 각각 swap하므로 순간적 불일치 가능 — avg 계산에 ±1 오차, 무시 가능
- Mutex는 TokioRuntimeSnapshot(3초 1회)에만 사용, hot path 무관
- 기존 어드민 JSON 포맷 유지 (어드민 코드 변경 최소화)

### 테스트 기준
- `cargo build --release` 워닝 0
- 기존 유닛테스트 전부 통과
- 2인 테스트: 스냅샷 포맷 동일 확인
- egress_drop, RTX 진단 카운터 정상 동작 확인
