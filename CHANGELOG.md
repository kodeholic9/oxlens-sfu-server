# Changelog

All notable changes to this project will be documented in this file.

Format follows [Keep a Changelog](https://keepachangelog.com/).

## [0.5.0] - 2026-03-07

### Added (Floor Control — MCPTT/MBCP 기반 PTT 시그널링)
- `src/room/floor.rs` — FloorController 상태 머신 (IDLE/TAKEN 2-state, T2/T_FLOOR_TIMEOUT 타이머)
- `config.rs` — RoomMode(Conference/Ptt), FLOOR_MAX_BURST_MS, FLOOR_PING_INTERVAL_MS, FLOOR_PING_TIMEOUT_MS
- `signaling/opcode.rs` — FLOOR_REQUEST(40), FLOOR_RELEASE(41), FLOOR_PING(42), FLOOR_TAKEN(141), FLOOR_IDLE(142), FLOOR_REVOKE(143)
- `signaling/message.rs` — FloorRequestMsg, FloorReleaseMsg, FloorPingMsg, RoomModeField
- `signaling/handler.rs` — handle_floor_request/release/ping, apply_floor_action, leave/cleanup 시 floor 퇴장 처리

### Changed
- `room/room.rs` — Room에 mode: RoomMode, floor: FloorController 필드 추가
- `room/room.rs` — RoomHub::create에 mode 파라미터 추가
- `signaling/handler.rs` — ROOM_CREATE/ROOM_LIST/ROOM_JOIN 응답에 mode 필드 포함
- `lib.rs` — 기본 방 첫 번째를 "무전 대화방" (PTT 모드)로 변경

## [0.4.0] - 2026-03-07

### Changed (Phase GM: GlobalMetrics 리팩터링)
- `ServerMetrics` → `GlobalMetrics` (Arc 공유, 전체 AtomicU64/AtomicU32)
- `EgressTimingAtomics` → `AtomicTimingStat`로 일반화 후 `GlobalMetrics`에 흡수
- spawn fan-out atomics 3개 (`spawn_rtp_relayed`, `spawn_sr_relayed`, `spawn_encrypt_fail`) → `GlobalMetrics`에 흡수
- `EnvironmentMeta`, `TokioRuntimeSnapshot` → `GlobalMetrics` 내부 필드로 통합
- ingress.rs: `handle_srtp`, `handle_subscribe_rtcp`, `handle_nack_block`, `relay_publish_rtcp` — `&mut self` → `&self` 복귀
- egress.rs: `send_twcc_to_publishers`, `send_remb_to_publishers` — `&mut self` → `&self`
- egress.rs: `run_egress_task` 시그니처 — `Arc<EgressTimingAtomics>` → `Arc<GlobalMetrics>`
- mod.rs: UdpTransport 필드 대폭 축소 (metrics/env_meta/tokio_snapshot/egress_timing/spawn atomics 제거 → `Arc<GlobalMetrics>` 1개)
- mod.rs: `from_socket`, `from_socket_with_id` — `pub` → `pub(crate)` (private type leak 해소)
- lib.rs: `Arc<GlobalMetrics>` 한 번 생성 → 모든 worker에 Arc::clone 공유
- 어드민 JSON 포맷 변경 없음 (어드민 코드 수정 불필요)

### Added (Phase MR: 모듈 분리)
- `src/metrics/mod.rs` — GlobalMetrics + AtomicTimingStat (관측 전용 모듈)
- `src/metrics/env.rs` — EnvironmentMeta
- `src/metrics/tokio_snapshot.rs` — TokioRuntimeSnapshot

### Removed
- `src/transport/udp/metrics.rs` — `src/metrics/`로 이동 (udp/ = 순수 미디어 파이프라인)
- `TimingStat` 구조체 (mutable accumulator → AtomicTimingStat으로 대체)
- `ServerMetrics` 구조체 (GlobalMetrics로 대체)
- `EgressTimingAtomics` 구조체 (AtomicTimingStat으로 일반화)

### 목적
- hot path 메서드의 `&mut self` 감염 제거 → `&self`로 복귀
- 메트릭 소유권 파편화 해소 (5가지 형태 → Arc<GlobalMetrics> 1개)
- 새 카운터 추가 시 보일러플레이트 최소화 (new/to_json/reset 3곳 → flush 1곳)
- transport/udp/ = 순수 미디어 코어, src/metrics/ = 관측 인프라 — 관심사 분리

## [0.3.10] - 2026-03-07

### Changed (Hot Path 병목 제거)
- `handle_srtp` fan-out: `room.other_participants()` Vec alloc → `room.participants.iter()` DashMap 직접 순회 (0 alloc)
- `relay_publish_rtcp`: 동일 Vec 할당 제거
- `handle_nack_block`: `all_participants().into_iter().find()` → `room.find_by_track_ssrc()` (0 alloc)
- `handle_subscribe_rtcp` RTCP relay: 동일 Vec 할당 제거

### Added
- `Room::find_by_track_ssrc()` — DashMap iter 직접 순회로 SSRC 기반 publisher 검색 (zero-alloc)
- `ServerMetrics::egress_drop` 카운터 — `try_send` 실패(큐 포화) 시 카운팅
  - `handle_srtp` fan-out, `relay_publish_rtcp` SR relay, `handle_nack_block` RTX 전송 3곳
- 어드민: RTCP grid에 `eg_drop` 표시 + egress_drop > 0 시 노란 경고 배너
- 어드민: 스냅샷에 egress_drop 포함
- SFU 패널 높이 280px → 420px

### 목적
- 패킷당 Vec alloc + Arc clone N회 제거로 hot path 메모리 압력 최소화
- egress 큐 포화 시 클라이언트 렉이 서버 내부 큐 문제인지 외부 네트워크 문제인지 구분 가능

## [0.3.9] - 2026-03-07

### Added (Phase TV: Telemetry Visibility — 텔레메트리 가시성 확보)

#### 서버
- **환경 메타데이터** (`metrics.rs: EnvironmentMeta`)
  - `build_mode` (`release`/`debug`, `cfg!(debug_assertions)` 컴파일 타임 결정)
  - `log_level` (`RUST_LOG` 또는 `LOG_LEVEL` 환경변수)
  - `worker_count`, `bwe_mode`, `version` — 서버 시작 시 1회 캡처
  - 3초 flush마다 `env` 섹션으로 JSON에 포함
- **Egress encrypt timing** (`metrics.rs: EgressTimingAtomics`)
  - egress task에서 encrypt 전/후 `Instant::now()` 측정 (~40ns/pkt)
  - `Arc<AtomicU64>` 3종 (sum, count, max) — lock-free, CAS max
  - 3초 flush 시 swap+JSON 생성 → `egress_encrypt` 섹션
- **Tokio RuntimeMetrics** (`metrics.rs: TokioRuntimeSnapshot`)
  - `.cargo/config.toml`에 `tokio_unstable` cfg 플래그 추가
  - 3초마다 `Handle::current().metrics()` atomic load → delta 계산
  - 1등급 (핵심): `busy_ratio`, `alive_tasks`, `global_queue_depth`, `budget_forced_yield_count`, `io_driver_ready_count`
  - 2등급 (per-worker): `busy_ratio`, `poll_count`, `steal_count`, `noop_count`
  - 3등급 (정보): `num_workers`, `num_blocking_threads`
  - hot path 비용 0 (3초에 1번 atomic load)

#### 어드민 대시보드
- SFU 패널에 **Egress Encrypt** 타이밍 표시 (avg/max ms)
- SFU 패널에 **Tokio Runtime** 섹션 추가
  - busy ratio (%), alive tasks, global queue, budget yield, io ready
  - per-worker 상세: busy%, polls, steals
  - 색상 임계값: >85% 노랑, >95% 빨강
- SFU 패널에 **Environment** 메타 표시 (version·build·bwe·workers·log_level)
- Contract 체크리스트에 `runtime_busy` 항목 추가 (85% WARN, 95% FAIL)
- 스냅샷에 egress_encrypt, env, tokio runtime, per-worker 상세 포함

### 설계 결정
- `tokio-metrics` 크레이트 미사용 — `Handle::current().metrics()` 직접 접근 (외부 의존성 0)
- `tokio_unstable`은 API 시그니처 불안정 표시이지 기능 불안정이 아님 (tokio 1.x 내 안정)
- Egress timing은 Atomic (not Mutex) — subscriber별 독립 task에서 경합 미미
- CAS loop max 패턴: `compare_exchange_weak` + `Relaxed` (정확도보다 성능 우선)

## [0.3.8] - 2026-03-07

### Added (Phase TW: TWCC Transport-Wide Congestion Control)

#### 서버
- **`transport/udp/twcc.rs`** 신규 모듈
  - `parse_twcc_seq(buf, extmap_id)` — RTP one-byte header extension에서 twcc seq# 추출
  - `TwccRecorder` — publisher별 twcc_seq → 도착 Instant 링버퍼 기록 (8192슬롯)
  - `build_twcc_feedback(recorder, media_ssrc)` — TWCC feedback RTCP 빌더
    - 2-bit status vector chunk (7 symbols/chunk)
    - recv_delta 인코딩 (small 1byte / large 2bytes, ×250µs)
    - reference_time 24-bit signed (×64ms), fb_pkt_count 자동 증가
    - pending_base 자동 전진 + 범위 초과 안전 가드
- `participant.rs`: `twcc_recorder: Mutex<TwccRecorder>` 필드 추가
- `ingress.rs`: hot path에 twcc seq 추출 + 도착 시간 기록 (Mutex 1회, O(1))
- `egress.rs`: `send_twcc_to_publishers()` — 100ms 주기 TWCC feedback 전송
- `config.rs`: `TWCC_EXTMAP_ID`(6), `TWCC_RECORDER_CAPACITY`(8192), `TWCC_FEEDBACK_INTERVAL_MS`(100) 등 6개 상수

### Changed
- `server_extmap_policy()`: transport-wide-cc extmap id=6 활성화
- REMB 타이머(1초) → TWCC 타이머(100ms)로 교체
- `send_remb_to_publishers()` → `send_twcc_to_publishers()` 전환
- `build_remb()` / `encode_remb_bitrate()` — `#[allow(dead_code)]` fallback 보존

### 목적
- Chrome GCC가 패킷 도착 시간 변화(delay gradient)를 분석하여 비트레이트 자율 결정
- 고정 REMB(500kbps) 대비 네트워크 상태 적응적 비트레이트 제어
- 업계 표준 congestion control 경로 (LiveKit, mediasoup 동일 패턴)

### 서버 Telemetry 연동
- `metrics.rs`: `twcc_sent` / `twcc_recorded` 카운터 — 3초 윈도우 flush
- `ingress.rs`: twcc record 성공 시 `twcc_recorded` 증가
- `egress.rs`: feedback 전송 성공 시 `twcc_sent` 증가

### 어드민 대시보드
- Contract 체크리스트: `twcc_feedback` — ⚠️ WARN(미구현) → ✅ PASS (`twcc_sent > 0`)
- SFU 패널 RTCP grid: `twcc_fb` / `twcc_rec` 카운터 표시
- 스냅샷: `twcc_fb` / `twcc_rec` 필드 포함

### 유닛테스트
- twcc.rs: 17개 (파서 5 + recorder 3 + chunk 3 + delta 3 + feedback 통합 4)
- 전체: 34개 PASS

## [0.3.7] - 2026-03-07

### Changed (udp.rs 모듈 분할 리팩토링)
- `transport/udp.rs` (53KB 단일 파일) → `transport/udp/` 디렉토리 5파일 분할
  - `mod.rs` (17KB): UdpTransport 본체 + run() + STUN + DTLS + worker
  - `ingress.rs` (16KB): handle_srtp + subscribe_rtcp + NACK + SR relay
  - `egress.rs` (6KB): egress task + PLI/REMB 전송
  - `rtcp.rs` (12KB): RTCP/RTP 파싱, RTX/PLI/REMB 빌더, 유틸
  - `metrics.rs` (6KB): TimingStat + ServerMetrics
- 기능 변경 없음 (순수 파일 분할, 동작 동일)
- `build_pli` pub re-export 유지 (handler.rs에서 참조)

### Added (Phase W-3: Subscriber Egress Task — LiveKit 패턴)

#### 서버
- `EgressPacket` enum (Rtp/Rtcp) + `Participant.egress_tx/rx` bounded channel
- `run_egress_task()` — subscriber별 전용 task, `outbound_srtp` 독점 encrypt
- subscribe DTLS ready 시 egress task 자동 spawn
- `handle_srtp` fan-out: spawn+Mutex → `egress_tx.try_send` (~50ns, lock 없음)
- `relay_publish_rtcp`: spawn+Mutex → `egress_tx.try_send` + async→fn
- `handle_nack_block` RTX: direct encrypt → `egress_tx.try_send` + async→fn
- `config.rs`: `EGRESS_QUEUE_SIZE=256` (bounded backpressure)

### 결과 (RPi 4B 벤치마크)
- 30인: loss **0.000%**, CPU 138%, latency **15ms** — 프로덕션 품질
- 35인: loss **7.0%** (W-2: 17.8%), latency 152ms — 2.5배 개선
- 40인: loss 22.8%, CPU 201% — RPi 4B 물리적 한계

### 설계 결정
- Mutex 경합 원천 제거: subscriber별 egress task가 outbound_srtp 독점
- bounded channel backpressure: 큐 풀 시 try_send 실패 = 드롭 (NACK/RTX 커버)
- egress_rx는 Mutex<Option>>로 1회용 .take() — 재 spawn 방지
- RPi 4B 실용 한계: 30~35인 (x86 서버로는 100인+ 가능한 구조)

## [0.3.6] - 2026-03-07

### Added (Phase W-2: SO_REUSEPORT multi-worker — UDP 멀티코어 분산 2단계)

#### 서버
- `bind_reuseport()` — socket2 SO_REUSEPORT UDP 바인드 (Linux 전용, 커널 4-tuple hash 분배)
- `resolve_worker_count()` — 0=auto(코어 수), `.env` `UDP_WORKER_COUNT`로 설정 가능
- `UdpTransport.worker_id` + `from_socket_with_id()` 생성자
- `lib.rs` — Linux: N개 SO_REUSEPORT worker spawn / Windows: single worker fallback
- REMB/metrics 타이머 worker-0 전담 (`is_primary` guard)
- DtlsSessionMap/ServerMetrics worker별 독립, RoomHub/ServerCert 공유(Arc)
- `Cargo.toml` — `socket2 = "0.5"` 의존성 추가
- `config.rs` — `UDP_WORKER_COUNT` 상수 (fallback, `.env` 우선)

### 결과 (RPi 4B 벤치마크)
- 30인: loss 1.3%→**0.1%**, CPU 118%→113% (4 recv 루프 분산 효과)
- 35인: loss 17.8%, CPU 155% (FAIL — outbound_srtp Mutex 경합 병목)

### 설계 결정
- Windows에는 SO_REUSEPORT 없음 → `#[cfg(target_os = "linux")]` 분기
- 35인+ 병목은 outbound_srtp Mutex 경합 — subscriber별 per-worker encrypt 또는 lock-free 구조 필요

## [0.3.5] - 2026-03-07

### Added (Phase W-1: Fan-out spawn — UDP 멀티코어 분산 1단계)

#### 서버
- `handle_srtp()` fan-out 루프를 `tokio::spawn`으로 분리 — 메인 루프는 recv→decrypt→cache→spawn만 수행
- `relay_publish_rtcp()` SR fan-out도 동일하게 `tokio::spawn` 분리
- `UdpTransport`에 `Arc<AtomicU64>` 3종 추가: `spawn_rtp_relayed`, `spawn_sr_relayed`, `spawn_encrypt_fail`
- `flush_metrics()`에서 spawn atomic 카운터를 JSON에 포함

### 결과 (RPi 4B 벤치마크)
- 25인: loss 0.004%→**0.000%**, CPU 80%→135% (4코어 균등 분산)
- 30인: loss **9.6%→1.3%**, CPU 121%→118% (**사실상 PASS, NACK/RTX 커버 가능**)
- 35인: loss 13.4%, CPU 155% (FAIL — recv+decrypt 단일 task 한계, W-2 필요)
- 4코어 균등 분산 확인: Cpu0~3 각 25~28% (이전: Cpu0만 91.6%)

### 설계 결정
- spawn 내부에서는 per-target 타이밍 생략 (ServerMetrics 접근 불가)
- relay total timing 제거 (spawn 후 의미 없음)
- outbound_srtp Mutex가 동시 접근 직렬화하므로 정합성 안전

## [0.3.4] - 2026-03-06

### Changed
- `ROOM_MAX_CAPACITY`: 20 → 1000 (벤치마크용)
- `ROOM_DEFAULT_CAPACITY`: 10 → 1000

### Added
- `doc/BENCHMARK-FANOUT-20260306.md` — RPi 4B fan-out 한계 테스트 (1pub→499sub, loss 0.002%, CPU 69%)
- `doc/BENCHMARK-CONFERENCE-20260306.md` — RPi 4B 회의실 한계 테스트 (25인 PASS, 30인 FAIL)

### Added (Media Quality: REMB + RR relay fix + JB delta)

#### 서버
- `build_remb()` + `encode_remb_bitrate()` — 서버 자체 REMB 패킷 생성 (24바이트, draft-alvestrand-rmcat-remb)
- `send_remb_to_publishers()` — 1초 주기 REMB 전송 (Chrome BWE 대역폭 힌트)
- `run()` 루프에 `remb_timer` 추가 (tokio::select 3분기)
- `config.rs`: `REMB_INTERVAL_MS`(1000), `REMB_BITRATE_BPS`(500000)
- subscribe RTCP 진단 카운터 3종 추가 (sub_rtcp_received/not_rtcp/decrypted)

### Fixed
- **RR relay metrics 카운터 버그** — plaintext RTCP PT에 `& 0x7F` 마스크 적용하여 `201 & 0x7F = 73 ≠ 201` 로 rr_relayed 항상 0
- **subscribe RTCP decrypt_fail 카운터 누락** — 복호화 실패 시 metrics에 반영 안 됨
- **transport-wide-cc extmap 제거** — TWCC 선언 시 Chrome BWE가 REMB 무시하여 available_bitrate 84kbps 고정

#### 클라이언트 SDK
- `jitterBufferDelay` delta 계산 전환 (누적값 → 3초 윈도우 평균 ms)
- `_prevStats.jb` Map 추가 (SSRC별 이전 delay/emitted 저장)

#### 어드민
- jb_delay 표시/판정을 SDK delta ms 직접 사용으로 변경 (3곳)

### 결과
- available_bitrate: 84kbps → 500kbps
- video bitrate: 32kbps → 470kbps
- quality_limit: bandwidth → none
- jb_delay: 170ms(가속) → 44~63ms(안정)
- Contract: 4/8 PASS → 7/8 PASS

### 설계 결정
- TWCC 구현 전까지 REMB 모드로 동작 (extmap에서 twcc 제외)
- REMB 500kbps는 로컬 테스트용, 실환경에서는 config에서 조정

## [0.3.3] - 2026-03-05

### Added (Phase T-4/5: 시계열 차트 + Contract + 스냅샷)

#### 어드민
- Canvas 시계열 차트 (3모드: 패킷 흐름/품질 지표/SFU 내부)
- WebRTC Contract 체크리스트 (8항목, PASS/FAIL/WARN)
- 스냅샷 내보내기 — 클립보드 복사 (Claude 분석용 텍스트)
- SFU 패널 헤더에 Contract/스냅샷 버튼
- 중앙 컨텐츠 상하 분할 (개요 테이블 + 시계열 차트)

## [0.3.2] - 2026-03-05

### Added (Phase T-3: 어드민 SFU 서버 패널)

#### 어드민
- `app.js`: `server_metrics` 타입 메시지 수신 처리 + `renderServerMetrics()`
- `index.html`: 우측 컨텐츠에 SFU 서버 패널 (h=260px, 보라색 돈)
- 타이밍 avg/max (ms), fan-out, RTCP 카운터, 실패 경고 표시

## [0.3.1] - 2026-03-05

### Added (Phase T-2: 서버 B구간 계측)

#### 서버
- `udp.rs`: `ServerMetrics` + `TimingStat` — relay/decrypt/encrypt/lock_wait 타이밍
- `udp.rs`: RTCP 카운터 (nack_received, rtx_sent, rtx_cache_miss, pli_sent, sr_relayed, rr_relayed)
- `udp.rs`: `run()` 루프 `tokio::select!` + 3초 타이머 → `flush_metrics()` → `admin_tx`
- `udp.rs`: hot path 메서드 `&self` → `&mut self` (metrics 기록용)
- `lib.rs`: `UdpTransport::from_socket()` 호출에 `admin_tx` 전달

### 설계 결정
- hot path에서 `Instant::now()` 만 사용 (allocation/Mutex 추가 없음)
- `ServerMetrics`는 UdpTransport 내부 단일 소유 (concurrent access 무)
- 3초 주기 flush 후 reset — 어드민이 담당 window 집계
- p95는 histogram 없이 정확 계산 불가 → max를 대신 노출

## [0.3.0] - 2026-03-05

### Added (Phase T-1: Media Telemetry 1단계)

#### 서버
- `opcode.rs`: `TELEMETRY = 30` (클라이언트 → 서버 telemetry 보고)
- `opcode.rs`: `ADMIN_TELEMETRY = 110` (서버 → 어드민, 향후 B구간용)
- `state.rs`: `admin_tx: broadcast::Sender<String>` — 어드민 WS broadcast 채널
- `handler.rs`: `handle_telemetry()` — 클라이언트 telemetry를 user_id/room_id 래핑하여 어드민으로 passthrough
- `handler.rs`: `admin_ws_handler()` — `/admin/ws` 어드민 전용 WebSocket
  - 접속 시 rooms/participants/tracks 스냅샷 전송
  - 이후 telemetry 스트림 실시간 중계
- `lib.rs`: `/admin/ws` 라우트 추가

#### 클라이언트 SDK
- `OP.TELEMETRY = 30` 추가
- `_sendSdpTelemetry()` — 방 입장 2초 후 SDP 전문 + m-line 요약 보고 (구간 S-1)
- `_parseMlineSummary()` — SDP에서 mid/kind/direction/codec/pt/ssrc 추출
- `_startStatsMonitor()` → 3초 주기 telemetry 수집+서버 전송 (기존 콘솔 로그 대체)
- `_collectPublishStats()` — 구간 A (outbound-rtp, candidate-pair)
- `_collectSubscribeStats()` — 구간 C (inbound-rtp, candidate-pair)
- `_collectCodecStats()` — 구간 S-2 (encoder/decoder 상태)

#### 어드민 대시보드 (완전 재작성)
- `/admin/ws` WebSocket 접속 + 자동 재연결
- Room 목록 패널 (참가자 상태 배지)
- 실시간 개요 테이블 (참가자/방향/kind/패킷/손실률/jitter/RTT/상태)
- 참가자 상세 패널 (코덱, publish/subscribe 수치)
- SDP 상태 패널 (m-line 요약)
- 시계열 버퍼 저장 (최근 100건, 향후 차트용)

### 설계 결정
- telemetry는 응답 없음 (fire-and-forget) — 비즈니스 로직 오염 방지
- admin broadcast channel (tokio::broadcast) — receiver 없으면 자동 버림
- SDP 전문은 S-1에서만 1회 전송, 3초 주기에는 stats만

## [0.2.3] - 2026-03-04

### Added (Phase C-3: Mute/Unmute Signaling)

#### 시그널링
- `MUTE_UPDATE` (op 17) — 클라이언트가 트랙 mute/unmute 상태 변경 요청
  - payload: `{ ssrc, muted }` (audio/video 구분 없이 SSRC 기반)
- `TRACK_STATE` (op 102) — 다른 참가자에게 mute 상태 브로드캐스트
  - payload: `{ user_id, ssrc, kind, muted }`

#### 서버
- `participant.rs`: `Track.muted: bool` 필드 추가 + `set_track_muted()` 메서드
- `handler.rs`: `handle_mute_update()` — 상태 갱신 + 브로드캐스트 + video unmute 시 PLI
- `state.rs`: `AppState.udp_socket: Arc<UdpSocket>` 추가 (handler에서 PLI 전송용)
- `udp.rs`: `UdpTransport::from_socket()` 생성자 + `build_pli()` pub 노출
- `lib.rs`: socket 생성 → AppState 공유 → UdpTransport 순서로 변경

### 목적
- 클라이언트 마이크/카메라 on/off 시 다른 참가자 UI에 상태 반영
- soft mute (track.enabled) / hard mute (replaceTrack(null)) 테스트용 서버 인프라
- video unmute 시 PLI로 키프레임 빠른 복구

## [0.2.2] - 2026-03-04

### Added (Phase C-2: RTCP Transparent Relay)

#### C-2a. Publish RTCP (SR) → subscriber fan-out (`udp.rs`)
- Publish PC에서 수신된 RTCP compound를 decrypt 후 모든 subscriber의 subscribe PC로 릴레이
- `relay_publish_rtcp()` — compound 통째로 전달 (NACK 없으므로 분리 불필요)
- RTP relay와 동일한 fan-out 패턴 (encrypt_rtcp → send_to)

#### C-2b/c/d. Subscribe RTCP (RR/PLI/REMB) → publisher relay (`udp.rs`)
- `split_compound_rtcp()` — compound RTCP 순회, NACK/relay 대상 분류
  - PT=205 (NACK): 기존 서버 RTX 처리 유지
  - PT=201 (RR), PT=206 FMT=1 (PLI), PT=206 FMT=15 (REMB): relay 대상
- media_ssrc 기반 publisher 매핑 → publisher별 RTCP compound 재조립
- `assemble_compound()` — relay 블록들을 compound로 이어붙임
- publisher의 publish PC outbound_srtp로 encrypt → send_to

#### 리팩터링
- `handle_subscribe_rtcp()` 전면 리팩터링 (compound 파싱 → 분기)
- `handle_nack_block()` — 기존 NACK→RTX 로직을 별도 메서드로 추출
- 미사용 `parse_ssrc()` 함수 제거

### Added (config.rs)
- `RTCP_PT_SR` (200), `RTCP_PT_RR` (201), `RTCP_PT_PSFB` (206)
- `RTCP_FMT_PLI` (1), `RTCP_FMT_REMB` (15)

### 목적
- Chrome congestion control에 SR/RR 피드백 제공 → 비트레이트 적응 개선
- 클라이언트 발 PLI/REMB → publisher 전달 → 화질/대역폭 제어 루프 완성
- NACK은 기존대로 서버에서 RTX 처리 (릴레이 안 함)

## [0.2.1] - 2026-03-04

### Added (Phase D: Hardening — 인증 제외)

#### D-1. Heartbeat timeout → disconnect (`handler.rs`)
- WS select 루프에 `interval_at` 타이머 추가 (30초 주기 체크)
- `last_activity: Instant` — 모든 WS 메시지 수신 시 갱신
- 90초 무활동 → `warn!` 로그 + break → cleanup 실행
- HEARTBEAT 핸들러에서 `participant.touch()` 호출 (zombie reaper 연동)

#### D-2. Zombie session reaper (`lib.rs` + `room.rs`)
- `RoomHub::reap_zombies(now_ms, timeout_ms)` — 전체 room 순회, `last_seen + 120s < now` 좀비 판별
- `run_zombie_reaper()` — 30초 주기 백그라운드 태스크, CancellationToken 연동
- 좀비 제거 시 남은 참가자에게 `tracks_update(remove)` + `participant_left` 브로드캐스트
- DTLS 미완료 상태 좀비도 동일 경로로 커버 (D-3 포함)

#### D-4. Graceful shutdown (`lib.rs`)
- `CancellationToken` 패턴 (tokio-util)
- `Ctrl+C` → axum `with_graceful_shutdown` + reaper cancel
- 3초 drain 대기 후 종료

#### D-5. 로그 레벨 정리 (`udp.rs` + `lib.rs`)
- hot-path detail 로그: `info!` → `debug!` (RTP, RELAY, RTCP, NACK, RTX)
- summary 로그: `info!` → `trace!`
- 기본 로그 레벨: `debug` → `info` (`RUST_LOG=light_livechat=debug`로 전환 가능)
- 운영 필수(연결/해제, STUN latch, DTLS 핸드셰이크, PLI, 에러)만 `info!` 유지

### Added (config.rs)
- `REAPER_INTERVAL_MS` (30,000) — 좀비 검사 주기
- `ZOMBIE_TIMEOUT_MS` (120,000) — 좀비 판정 시간 (WS 90s + 마진 30s)
- `SHUTDOWN_DRAIN_MS` (3,000) — graceful shutdown drain 대기

### Added (dependencies)
- `tokio-util = "0.7"` (CancellationToken)

## [0.2.0] - 2026-03-04

### Added (Phase C: NACK-based RTX Retransmission)

#### 서버 (light-livechat)
- **RtpCache** — 비디오 RTP plaintext 링버퍼 캐시 (128슬롯, seq % 128 인덱싱)
  - `participant.rs`에 `RtpCache` 구조체 추가 (`store()`, `get()` + seq 검증)
  - `Participant`에 `rtp_cache: Mutex<RtpCache>` 필드 추가
  - `udp.rs` hot path: VP8(PT=96) decrypt 후 캐시 저장
- **NACK 파싱** (RFC 4585 Generic NACK, PT=205, FMT=1)
  - `parse_rtcp_nack()` — RTCP compound 패킷에서 NACK FCI 추출
  - `expand_nack(pid, blp)` — PID + BLP 비트마스크 → 손실 seq 목록
  - Subscribe PC RTCP 처리: `handle_subscribe_rtcp()` 메서드 신설
- **RTX 패킷 조립** (RFC 4588)
  - `build_rtx_packet()` — PT=97, rtx_ssrc, OSN(2바이트) + 원본 페이로드
  - Publisher별 `rtx_seq: AtomicU16` 카운터
  - 캐시 조회 → RTX 조립 → subscribe outbound SRTP 암호화 → send_to
- **RTX SSRC 자동 할당**
  - `Track.rtx_ssrc: Option<u32>` 필드 추가 (video only)
  - `add_track()`: video 트랙 등록 시 `media_ssrc + 1000 + counter`로 RTX SSRC 할당
  - `tracks_update`, `ROOM_JOIN` 응답에 `rtx_ssrc` 필드 포함
- **config.rs** 상수 추가: `RTP_CACHE_SIZE`(128), `RTX_PAYLOAD_TYPE`(97), `RTCP_PT_NACK`(205), `RTCP_FMT_NACK`(1)
- 로그 태그: `[DBG:NACK]`, `[DBG:RTX]`, `[DBG:SUB]`

#### 클라이언트 (insight-lens)
- **sdp-builder.mjs**: subscribe SDP video m-line에 RTX SSRC 선언
  - `a=ssrc-group:FID {video_ssrc} {rtx_ssrc}`
  - `a=ssrc:{rtx_ssrc} cname:light-sfu`
- SDK: 서버가 보내는 `rtx_ssrc` 필드가 spread로 자동 전달됨 (별도 수정 불필요)

### 목적
- 패킷 손실 시 Chrome NACK → 서버 캐시에서 RTX 재전송 (publisher 관여 없이 RTT 절반)
- 비디오 화질 안정성 향상 (블록 노이즈, 프레임 깨짐 감소)

### 확인 방법
- 서버 로그: `[DBG:NACK]` (손실 감지) + `[DBG:RTX]` (재전송)
- Chrome `chrome://webrtc-internals`: subscribe PC inbound-rtp의 `nackCount`, `packetsLost`
- 인위적 테스트: clumsy 등으로 UDP 패킷 drop → NACK 빈발 → RTX 동작 확인

## [0.1.7] - 2026-03-04

### Added (Phase A-3: PLI Keyframe Request)
- `build_pli(media_ssrc)` — RTCP PLI 패킷 빌더 (12바이트 고정, RFC 4585)
- `SrtpContext::encrypt_rtcp()` — SRTCP 암호화 메서드 추가
- `send_pli_to_publishers()` — room 내 모든 publisher에게 PLI 전송
- Subscribe PC SRTP ready 시점에 자동 PLI 트리거 (udp.rs DTLS 핸드셰이크 완료 콜백)
- `start_dtls_handshake()` spawn에 socket + room_hub 전달 (PLI 전송용)
- `[DBG:PLI]` 로그 태그 추가

### 목적
- 새 구독자 입장 시 VP8 키프레임 대기 10~20초 → 1~2초로 단축
- publisher 브라우저에 PLI 전송 → 즉시 키프레임 생성 → 서버 relay → 구독자 디코더 시작

## [0.1.5] - 2026-03-04

### Changed (Phase A-1: 2PC / SDP-free Architecture)

**아키텍처 전면 전환 — 서버 SDP-free + 2PC 구조**

#### 핵심 변경
- **PC 1개 → 2개 (publish + subscribe) 분리**
  - `PcType` enum: `Publish` | `Subscribe`
  - `MediaSession` 구조체: ufrag, ice_pwd, address, inbound/outbound SRTP (PC당 1세트)
  - `Participant`가 `publish: MediaSession` + `subscribe: MediaSession` 소유
- **서버 SDP 제거**
  - `transport/sdp.rs` 삭제 (파서 + 빌더 + 20개 테스트 전부)
  - ROOM_JOIN이 SDP answer 대신 `server_config` JSON 응답
  - 코덱/PT/extmap은 서버 정책으로 고정 (협상 아닌 통보)
- **Room 3-index가 PcType 포함**
  - `by_ufrag: DashMap<ufrag, (Participant, PcType)>` — STUN latch 시 PC 종류 식별
  - `by_addr: DashMap<addr, (Participant, PcType)>` — 미디어 수신 시 PC 종류 식별
  - RoomHub 역인덱스도 참가자당 2개 ufrag 등록

#### 시그널링 변경
- `SDP_OFFER` (op 15), `ICE_CANDIDATE` (op 16) 제거
- `PUBLISH_TRACKS` (op 15) 추가 — 클라이언트가 자기 트랙 SSRC 등록
- `TRACKS_UPDATE` (op 101) 추가 — 트랙 추가/제거 통보 (subscribe re-nego 트리거)
- `TRACK_EVENT`, `SERVER_ICE_CANDIDATE` 제거

#### 미디어 경로 변경
- UDP STUN latch 시 PcType 식별 → 해당 세션의 ice_pwd로 검증
- DTLS 핸드셰이크를 PC session별 독립 수행 + SRTP 키 독립 설치
- 미디어 수신: publish PC addr에서만 처리
- 미디어 전송: target의 subscribe PC addr로 전송

#### 제거
- `transport/sdp.rs` — 서버가 SDP를 모르므로 전체 삭제
- `state.rs`에서 `Router` 제거 (sockaddr 기반 직접 릴레이)

#### 유지 (검증 계층)
- `transport/demux.rs` — RFC 5764 분류기
- `transport/demux_conn.rs` — Conn trait 어댑터
- `transport/dtls.rs` — DTLS 핸드셰이크
- `transport/srtp.rs` — SRTP 암복호화
- `transport/stun.rs` — STUN 파서/빌더
- `transport/ice.rs` — ICE credential 생성

## [0.1.4] - 2026-03-04

### Added (Phase 3.5: Debug Logging + Video Rendering Fix)
- Debug log system: 6 server tags (`[DBG:SDP]`, `[DBG:STUN]`, `[DBG:DTLS]`, `[DBG:RTP]`, `[DBG:RELAY]`, `[DBG:RTCP]`) + 4 client tags (`[DBG:SDP]`, `[DBG:ICE]`, `[DBG:TRACK]`, `[DBG:RTP]`)
- RTP header parser for logging (SSRC, PT, seq, timestamp, marker, payload_len)
- Log throttling: AtomicU64 counter — first 50 packets detailed, then summary every 1000
- Config constants: `DBG_DETAIL_LIMIT`, `DBG_SUMMARY_INTERVAL`
- Client: periodic inbound-rtp stats monitor (3s interval via `getStats()`)
- Client: ICE state transition logging (iceConnectionState, connectionState, gatheringState, local candidates)
- Client: ontrack detail logging (kind, id, readyState, stream.id, mid) + mute/unmute/ended events

### Fixed
- **Video rendering**: ontrack fires before grid tile exists → `remoteVideoStream` stored in app state, `tryAttachRemoteVideo()` called from both `media:track`, `room:joined`, and `room:event(participant_joined)` — works regardless of event ordering

### Known Issues (deferred to Phase 4)
- Low video quality: RTCP not relayed → Chrome congestion control has no feedback → conservative bitrate
- Slow first-frame for early joiner: no PLI sent when new participant enters
- SSRC→user_id mapping absent: limits 3+ participant support

### Added (Phase 3: SDP Negotiation)
- SDP Offer parsing (`transport/sdp.rs`): media section extraction, codec/extmap capture, SSRC collection, direction/rtcp-rsize detection
- SDP Answer generation: ICE-Lite params, passive DTLS fingerprint, codec mirror, host candidate
- Local IP auto-detection (routing table based, `detect_local_ip()`)
- ROOM_JOIN now accepts `sdp_offer` (required) and returns `sdp_answer`
- Unit tests: 20 tests covering parse/answer/utility

### Changed
- `RoomJoinRequest.sdp_offer`: `Option<String>` → `String` (required field)
- ROOM_JOIN response: removed separate `ice_params` + `dtls_fingerprint`, replaced with unified `sdp_answer`
- SDP design: direction echo (sendrecv → sendrecv), no SSRC/msid in answer, re-nego deferred

## [0.1.3] - 2026-03-03

### Added
- UDP transport ↔ RoomHub full integration
- STUN cold path: `parse()` → `latch_by_ufrag()` → MESSAGE-INTEGRITY verify → Binding Response
- USE-CANDIDATE triggers DTLS handshake (10s timeout, spawned async task)
- DTLS complete → `export_srtp_keys()` → `participant.install_srtp_keys()`
- SRTP hot path: `find_by_addr()` O(1) → decrypt → fan-out → encrypt → send_to
- RTCP detection (RFC 5761 PT 72-79): decrypt for logging, not relayed
- DemuxConn channel switched to `Bytes` (reduced heap allocation)
- Periodic stale DTLS session cleanup (every 1000 packets)
- DTLSConn keepalive recv loop (prevents session drop)

### Changed
- UDP transport no longer uses standalone IceCredentials — all lookup via RoomHub
- Signaling handler: cleaned unused imports, fixed socket variable naming

## [0.1.2] - 2026-03-03

### Added
- DTLS module (`transport/dtls.rs`): server certificate generation, SHA-256 fingerprint, passive handshake, SRTP key derivation (RFC 5764 §4.2)
- SRTP module (`transport/srtp.rs`): encrypt/decrypt context using webrtc-srtp, AES_CM_128_HMAC_SHA1_80 profile, roundtrip unit test
- DemuxConn adapter (`transport/demux_conn.rs`): bridges demux UDP loop with DTLSConn via mpsc channel + webrtc-util Conn trait
- Participant media fields: ICE ufrag/pwd, latched address, inbound/outbound SRTP contexts, published tracks
- Room 3-index lookup: `participants` (user_id), `by_ufrag` (STUN cold path), `by_addr` (SRTP hot path) — all O(1) via DashMap
- RoomHub reverse indices: `ufrag_index` (ufrag → room_id), `addr_index` (addr → room_id) — O(1) cross-room lookup
- Room.latch(): STUN address latching with NAT rebinding support
- ServerCert in AppState (DTLS fingerprint available for SDP answer)
- Signaling handler: IDENTIFY, ROOM_CREATE, ROOM_JOIN, ROOM_LEAVE, MESSAGE fully implemented
- ROOM_JOIN returns ice_params + dtls_fingerprint for browser ICE/DTLS initiation
- Participant join/leave broadcast (ROOM_EVENT) to other room members
- Message relay (MESSAGE → MESSAGE_EVENT broadcast)
- WS disconnect cleanup (reverse index cleanup + leave notification)

### Changed
- Switched from webrtc-dtls 0.12 to dtls 0.17.1 (aligned with mini-livechat 0.17.x ecosystem)
- webrtc-util 0.11 → 0.17.1, added webrtc-srtp 0.17.1
- Added bytes, sha2, async-trait dependencies
- AppState::new() now requires ServerCert parameter
- UDP transport refactored: peer channel uses Vec<u8> instead of DemuxPacket struct

## [0.1.1] - 2026-03-03

### Added
- STUN parser/builder (RFC 8489): binding request parsing, success response generation
- MESSAGE-INTEGRITY (HMAC-SHA1) verification and generation
- FINGERPRINT (CRC32) attribute support
- XOR-MAPPED-ADDRESS encoding (IPv4/IPv6)
- ICE-Lite handler: ufrag validation, binding response, USE-CANDIDATE detection
- UDP transport: single-port listener with RFC 5764 demux dispatch
- `hmac`, `sha1`, `crc32fast`, `getrandom` dependencies

## [0.1.0] - 2026-03-03

### Added
- Project skeleton with module structure
- Signaling protocol design (opcode-based, pid sequential pairing)
- WebSocket handler with opcode dispatch (stub responses)
- Packet demultiplexer for Bundle (RFC 5764 first-byte classification)
- Room and Participant state management with DashMap
- SSRC-based media routing table (Router)
- Error type hierarchy (1xxx~9xxx)
- Configuration constants
- README, CHANGELOG, TODO documentation
