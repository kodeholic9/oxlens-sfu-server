# Light-SFU Worker Optimization Benchmark Report

> 2026-03-07 — Raspberry Pi 4B 대상 Phase W (멀티코어 분산) 최적화 벤치마크

## 1. 테스트 개요

### 목적
Conference 30인 FAIL(v0.3.4, loss 9.6%)을 해결하기 위한 3단계 최적화의
각 단계별 효과를 정량적으로 측정하고, RPi 4B에서의 실용 한계를 확정한다.

### 최적화 단계

| Phase | 버전 | 핵심 변경 |
|-------|------|----------|
| W-1 | v0.3.5 | fan-out spawn — encrypt+send를 tokio::spawn 분리 |
| W-2 | v0.3.6 | SO_REUSEPORT — N개 독립 recv 루프, 커널 4-tuple hash 분배 |
| W-3 | v0.3.7 | subscriber egress task — subscriber별 독립 encrypt pipeline (LiveKit 패턴) |

### 최적화 전략의 배경 (업계 선행 사례)

| 플랫폼 | 한 방 내 확장 전략 | 언어 |
|--------|-----------------|------|
| **LiveKit** | subscriber별 egress goroutine (→ W-3과 동일) | Go |
| **mediasoup** | 싱글스레드 이벤트루프, Room 분할 | C++ |
| **Zoom** | 대규모는 MCU(합성) 전환 | C/C++ |
| **Discord** | SFU cascading (서버 체이닝) | Rust |

W-3의 subscriber별 egress task는 LiveKit의 검증된 패턴을 Rust/Tokio로 이식한 것이다.

### 테스트 환경

| 항목 | 사양 |
|------|------|
| **서버** | Raspberry Pi 4B (4-core ARM Cortex-A72 @ 1.8GHz, 4GB RAM, GbE) |
| **서버 SW** | light-livechat v0.3.5~v0.3.7 (Rust + Tokio + Axum) |
| **벤치 클라이언트** | Windows PC (sfu-bench v0.2.0, conference mode) |
| **네트워크** | 동일 LAN, 유선 Gigabit Ethernet |
| **RTP 설정** | 30fps, 1200B payload, 30초 duration |

---

## 2. 병목 진단 (v0.3.4 baseline)

### 30인 FAIL 원인

```
Cpu0: 73.1% us + 18.5% sy = 91.6%  ← 포화
Cpu1:  0.2%
Cpu2:  5.2%
Cpu3:  0.0%
```

UDP hot loop(`recv → decrypt → encrypt×29 → send×29`)가 단일 async task로 실행되어
Cpu0 한 코어에 집중. 나머지 3코어 유휴.

### 처리 시간 구성 (30인, 패킷 1개당)

```
recv_from:       ~10µs
decrypt:         ~30µs
fan-out(×29):
  encrypt×29:  ~1,450µs  (50µs × 29)  ← 전체의 95%
  send_to×29:    ~290µs  (10µs × 29)
─────────────────────────
total:         ~1,780µs / pkt

input 900 pps × 1.78ms = 1,600ms CPU/s → 1코어 160% → 포화
```

---

## 3. Phase W-1: Fan-out Spawn (v0.3.5)

### 변경 내용
- `handle_srtp()` fan-out 루프를 `tokio::spawn`으로 분리
- `relay_publish_rtcp()` SR fan-out도 동일 적용
- 메인 루프: `recv → decrypt → cache → spawn` 만 수행, 즉시 다음 패킷 처리
- spawn된 task는 Tokio work-stealing으로 유휴 코어에 분산

### 결과

```
┌──────────┬────┬───────┬─────────┬────────┬──────────┬──────────┬──────┬──────┐
│ label    │  N │ loss% │ avg(ms) │ p95(ms) │ max(ms) │ out pps  │ CPU% │ 판정 │
├──────────┼────┼───────┼─────────┼────────┼──────────┼──────────┼──────┼──────┤
│ w1-25p   │ 25 │ 0.000 │   12.73 │  35.12 │    77.72 │  17,963 │  135 │ PASS │
│ w1-30p   │ 30 │ 1.286 │   35.42 │ 142.19 │   299.98 │  25,763 │  118 │ PASS*│
│ w1-35p   │ 35 │13.378 │  249.92 │ 448.97 │   542.17 │  31,383 │  155 │ FAIL │
└──────────┴────┴───────┴─────────┴────────┴──────────┴──────────┴──────┴──────┘
* 30인 PASS: loss 1.3%는 NACK/RTX로 커버 가능 영역
```

### CPU 코어 분산 (25인)

```
변경 전 (v0.3.4):            W-1 (v0.3.5):
  Cpu0: 91.6%                  Cpu0: 22.7%
  Cpu1:  0.2%                  Cpu1: 26.2%
  Cpu2:  5.2%                  Cpu2: 25.9%
  Cpu3:  0.0%                  Cpu3: 24.8%
```

**4코어 균등 분산 달성.** Cpu0 독점 → 4코어 25%씩.

### 분석
- encrypt+send를 spawn으로 분리하여 Tokio work-stealing 활성화
- recv+decrypt는 여전히 단일 task — 35인에서 이것이 병목

---

## 4. Phase W-2: SO_REUSEPORT Multi-worker (v0.3.6)

### 변경 내용
- `socket2` SO_REUSEPORT로 동일 포트에 4개 소켓 바인드 (Linux 전용)
- 커널 4-tuple hash로 패킷 분배 → 같은 클라이언트는 항상 같은 worker
- Worker별 독립: DtlsSessionMap, ServerMetrics
- Worker 공유: RoomHub(Arc), ServerCert(Arc)
- REMB/metrics 타이머 worker-0 전담
- Windows fallback: single worker (기존 동작)

### 결과

```
┌──────────┬────┬───────┬─────────┬────────┬──────────┬──────────┬──────┬──────┐
│ label    │  N │ loss% │ avg(ms) │ p95(ms) │ max(ms) │ out pps  │ CPU% │ 판정 │
├──────────┼────┼───────┼─────────┼────────┼──────────┼──────────┼──────┼──────┤
│ w2-30p   │ 30 │ 0.100 │   29.79 │ 168.48 │   277.10 │  25,997 │  113 │ PASS │
│ w2-35p   │ 35 │17.811 │  297.09 │ 410.78 │   623.61 │  29,409 │  156 │ FAIL │
└──────────┴────┴───────┴─────────┴────────┴──────────┴──────────┴──────┴──────┘
```

### CPU 코어 분산 (30인)

```
W-1 (v0.3.5):                W-2 (v0.3.6):
  Cpu0~3: 각 ~29%              Cpu0: 20.0%
  total: ~118%                 Cpu1: 20.9%
                               Cpu2: 21.4%
                               Cpu3: 21.7%
                               total: ~113%
```

### 분석
- 30인: recv+decrypt도 4 worker에 분산되어 loss 1.3% → 0.1%
- 35인: **W-1보다 악화** (13.4% → 17.8%) — outbound_srtp Mutex 경합 증가
  - 4 worker가 동시에 같은 subscriber의 outbound_srtp Mutex를 경합
  - recv 분산의 이득 < Mutex 경합의 손실

---

## 5. Phase W-3: Subscriber Egress Task (v0.3.7)

### 변경 내용 (LiveKit 패턴)
- `EgressPacket` enum (Rtp/Rtcp) + subscriber별 bounded channel (256)
- `run_egress_task()`: subscriber 전용 task, outbound_srtp 독점 encrypt → send
- subscribe DTLS ready 시 egress task 자동 spawn
- fan-out: `spawn + Mutex.lock → encrypt` → `egress_tx.try_send` (~50ns, lock 없음)
- relay_publish_rtcp, handle_nack_block: 동일 egress 경유

### 아키텍처 변화

```
[W-2] Worker recv → decrypt → spawn { Mutex.lock → encrypt → send }
       ↑ 4 workers가 동일 subscriber의 Mutex를 경합 (35인 17.8%)

[W-3] Worker recv → decrypt → egress_tx.try_send (~50ns)
                                    ↓
                      Egress task: rx.recv → encrypt(독점) → send
                      ↑ subscriber당 1개, Mutex 경합 원천 제거
```

### 결과

```
┌──────────┬────┬───────┬─────────┬─────────┬──────────┬──────────┬──────┬──────┐
│ label    │  N │ loss% │ avg(ms) │ p95(ms) │ max(ms)  │ out pps  │ CPU% │ 판정 │
├──────────┼────┼───────┼─────────┼─────────┼──────────┼──────────┼──────┼──────┤
│ w3-30p   │ 30 │ 0.000 │   15.45 │   36.55 │    89.05 │  25,975 │  138 │ PASS │
│ w3-35p   │ 35 │ 7.033 │  152.14 │  342.05 │   485.63 │  32,779 │  157 │ FAIL*│
│ w3-40p   │ 40 │22.805 │  274.40 │  391.12 │   789.22 │  35,627 │  201 │ FAIL │
└──────────┴────┴───────┴─────────┴─────────┴──────────┴──────────┴──────┴──────┘
* 35인: loss 7%는 NACK/RTX 커버 경계 (실사용 시 영상 끊김 가능)
```

### CPU 코어 분산 (30인)

```
W-2 (v0.3.6):                W-3 (v0.3.7):
  Cpu0~3: 각 ~21%              Cpu0: 22.9%
  total: ~113%                 Cpu1: 27.6%
                               Cpu2: 28.0%
                               Cpu3: 29.2%
                               total: ~138%
```

총 CPU는 증가했으나 loss 0%, latency 절반. egress task가 encrypt를 유휴 코어에서
독립 실행하면서 전체 처리량이 증가한 것이다.

### 분석
- 30인: **loss 0.000%, latency 15ms** — 프로덕션 품질 달성
- 35인: W-2 대비 loss 17.8% → 7.0% (2.5배 개선), latency 297ms → 152ms (절반)
- 40인: CPU 201% — RPi 4B ARM 1.8GHz의 물리적 한계

---

## 6. 전체 진화 비교표

### 30인 Conference

```
┌─────────┬────────┬───────┬─────────┬────────┬──────┬──────────────────────────┐
│ 버전    │  Phase │ loss% │ avg(ms) │ p95(ms)│ CPU% │ 핵심 변경                │
├─────────┼────────┼───────┼─────────┼────────┼──────┼──────────────────────────┤
│ v0.3.4  │ base   │  9.60 │   67.45 │  85.98 │  121 │ single loop              │
│ v0.3.5  │ W-1    │  1.29 │   35.42 │ 142.19 │  118 │ fan-out spawn            │
│ v0.3.6  │ W-2    │  0.10 │   29.79 │ 168.48 │  113 │ SO_REUSEPORT 4 workers   │
│ v0.3.7  │ W-3    │  0.00 │   15.45 │  36.55 │  138 │ egress task (LiveKit)    │
└─────────┴────────┴───────┴─────────┴────────┴──────┴──────────────────────────┘
```

### 35인 Conference

```
┌─────────┬────────┬───────┬─────────┬────────┬──────┐
│ 버전    │  Phase │ loss% │ avg(ms) │ p95(ms)│ CPU% │
├─────────┼────────┼───────┼─────────┼────────┼──────┤
│ v0.3.5  │ W-1    │ 13.38 │  249.92 │ 448.97 │  155 │
│ v0.3.6  │ W-2    │ 17.81 │  297.09 │ 410.78 │  156 │
│ v0.3.7  │ W-3    │  7.03 │  152.14 │ 342.05 │  157 │
└─────────┴────────┴───────┴─────────┴────────┴──────┘
```

### RPi 4B 최대 처리량 (W-3, v0.3.7)

```
30인 PASS:  25,975 pps / ~249 Mbps (loss 0.000%)
35인 WARN:  32,779 pps / ~315 Mbps (loss 7.0%)
40인 FAIL:  35,627 pps / ~342 Mbps (loss 22.8%, CPU 201%)
```

---

## 7. 결론

### 7.1 핵심 수치

| 항목 | 결과 |
|------|------|
| **최대 PASS (loss 0%)** | 30인 회의 (870 스트림, 15ms) |
| **이전 한계 (v0.3.4)** | 25인 (30인 loss 9.6% FAIL) |
| **개선 폭** | 30인 loss: 9.6% → 0.000% |
| **latency 개선** | 30인: 67ms → 15ms (4.5배) |
| **RPi 4B 실용 한계** | 30~35인 (35인은 NACK/RTX 의존) |

### 7.2 각 Phase의 기여

| Phase | 30인 loss 변화 | 핵심 효과 |
|-------|---------------|----------|
| W-1 | 9.6% → 1.3% | encrypt 부하를 유휴 코어에 분산 |
| W-2 | 1.3% → 0.1% | recv+decrypt 부하도 분산 |
| W-3 | 0.1% → 0.0% | Mutex 경합 원천 제거, latency 절반 |

### 7.3 아키텍처 검증

W-3(subscriber 별 egress task)는 LiveKit의 검증된 패턴과 동일하다.
Rust/Tokio의 async task는 Go의 goroutine과 동등한 경량 동시성을 제공하며,
subscriber 35개 × egress task 35개가 RPi 4코어 위에서 효율적으로 스케줄링된다.

이 구조는 x86 서버(8코어+, 4GHz+)에서 100인+ conference가 가능한 기반이다.

### 7.4 한계 및 향후 과제

| 과제 | 설명 |
|------|------|
| x86 벤치마크 | 50인+ conference 한계 측정 |
| udp.rs 파일 분리 | ingress.rs / egress.rs / metrics.rs / rtcp.rs / rtp.rs |
| TWCC 구현 | REMB 모드 → TWCC 전환, 대역폭 적응 개선 |
| Simulcast | 해상도별 다중 스트림, 대규모 회의 대역폭 절감 |

---

## 8. 실행 명령어 기록

```bash
# 서버 설정: LOG_LEVEL=info, ROOM_CAPACITY=1000, UDP_WORKER_COUNT=0(auto=4)
# 서버: RPi 4B, 클라이언트: Windows PC (동일 LAN)

# --- Phase W-1 (v0.3.5) ---
sfu-bench --server 192.168.0.29 --ws-port 1974 --mode conference \
          --participants 25 --duration 30 --label w1-25p

sfu-bench --server 192.168.0.29 --ws-port 1974 --mode conference \
          --participants 30 --duration 30 --label w1-30p

sfu-bench --server 192.168.0.29 --ws-port 1974 --mode conference \
          --participants 35 --duration 30 --label w1-35p

# --- Phase W-2 (v0.3.6) ---
sfu-bench --server 192.168.0.29 --ws-port 1974 --mode conference \
          --participants 30 --duration 30 --label w2-30p

sfu-bench --server 192.168.0.29 --ws-port 1974 --mode conference \
          --participants 35 --duration 30 --label w2-35p

# --- Phase W-3 (v0.3.7) ---
sfu-bench --server 192.168.0.29 --ws-port 1974 --mode conference \
          --participants 30 --duration 30 --label w3-30p

sfu-bench --server 192.168.0.29 --ws-port 1974 --mode conference \
          --participants 35 --duration 30 --label w3-35p

sfu-bench --server 192.168.0.29 --ws-port 1974 --mode conference \
          --participants 40 --duration 30 --label w3-40p
```

---

*author: kodeholic (powered by Claude)*
*sfu-bench v0.2.0 / light-livechat v0.3.5~v0.3.7*
