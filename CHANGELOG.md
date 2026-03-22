# Changelog

All notable changes to this project will be documented in this file.

> **명칭 변경**: v0.5.2까지는 `light-livechat` / `livechatd`로 개발.
> v0.5.3부터 `oxlens-sfu-server` / `oxsfud`로 현행화.

Format follows [Keep a Changelog](https://keepachangelog.com/).

## [0.6.4] - 2026-03-22

### Fixed (PTT Video Freeze — WARM 복귀 시 영상 정지)

#### Video dynamic ts_gap 추가 (ptt_rewriter.rs)
- WARM idle 후 발화 시 arrival_time gap과 RTP ts gap 불일치 → jitter buffer 교란 방지
- Audio와 동일 원리: idle 경과 시간 × 90kHz → video ts_gap 동적 계산
- 이전: 고정 3000 (33ms) → idle 12초 후에도 ts 33ms만 점프 → jitter 폭등
- 수정: idle 12초 → ts_gap=1,080,000 (12초분) → arrival gap과 일치

#### Video marker bit 강제 설정 제거 (ptt_rewriter.rs)
- 화자 전환 첫 패킷에 marker=1 강제 설정 → Audio 전용으로 변경
- VP8 키프레임은 다중 RTP 패킷 분할 → 첫 패킷에 marker=1 시
  Chrome이 "이 패킷이 전체 프레임"으로 오인 → 불완전 디코딩 → freeze
- Audio(Opus)는 1패킷=1프레임이라 marker 강제가 무해 → 유지

#### rewrite() first_pkt video base 재계산 제거 + pending 보상 (ptt_rewriter.rs)
- 기존: first_pkt에서 last_virtual_ts + 3000(고정)으로 v_base 재계산
- 수정: switch_speaker에서 dynamic ts_gap으로 설정한 v_base를 기본으로 사용
- Video pending 보상: switch_speaker → first_pkt 경과 시간(getUserMedia 대기) × 90kHz 가산
  arrival_gap = idle + pending, ts_gap = idle + pending → subscriber jb_delay 폭등 방지

### Added (디버그)
- `diagnose_vp8()` 함수 + `Vp8Diag` 구조체 (ptt_rewriter.rs)
- `[DIAG:VP8:PENDING]` / `[DIAG:VP8:KEYFRAME]` 조건부 진단 로그 (ingress.rs)
- TODO: 안정화 후 제거

## [0.6.3] - 2026-03-21

### Added (Floor Control v2 — priority + queue + preemption)

#### FloorController 전면 재작성 (room/floor.rs)

- `FloorState::Speaking { speaker_priority }` — 발화자 우선순위 추적
- `QueueEntry { user_id, priority, enqueued_at }` — 대기열 엔트리
- `VecDeque` 기반 우선순위 큐 (priority 내림차순 삽입)
- `FloorAction::Queued` variant — 큐 삽입 결과 반환
- `Vec<FloorAction>` 반환 — 단일 요청에서 복수 액션 발생 가능 (preemption: Revoked + Granted)
- `FloorConfig`: max_queue_size, preemption_enabled, max_burst_ms, ping_timeout_ms
- Preemption: 높은 priority 요청 시 현재 발화자 revoke + 즉시 grant
- Queue pop: release/revoke/timer 후 큐에서 자동 grant
- `queue_snapshot()`, `queue_size()`, `query_queue_position()` 조회 API
- 12개 유닛 테스트 (큐잉, 선점, 큐 풀, 중복 방지, 타이머 pop 등)

#### 시그널링 확장

- `FLOOR_QUEUE_POS` (op=43) — 큐 위치 조회 opcode
- `FloorRequestMsg.priority` 필드 (0~255, optional, 기본 0)
- `FloorQueuePosMsg` 신규 메시지 타입
- ROOM_JOIN/ROOM_SYNC: `floor.queue` 스냅샷 + `speaker_priority` 필드
- `apply_floor_actions()`: Vec<FloorAction> 일괄 처리 (queue pop → 개별 Granted WS 전송)

#### MBCP (UDP) 경로

- MBCP Floor Request: `FLOOR_DEFAULT_PRIORITY` 사용
- `apply_mbcp_floor_actions()`: Vec 처리 + preemption 카운터

#### GlobalMetrics

- `ptt_floor_queued` — 큐 삽입 횟수
- `ptt_floor_preempted` — 선점 횟수
- `ptt_floor_queue_pop` — 큐 자동 grant 횟수

### Changed

- `floor_ops.rs` 전면 재작성 (Vec<FloorAction> 처리)
- `tasks.rs` 전면 재작성 (check_timers Vec 처리 + 큐 pop 후속 브로드캐스트)
- `room_ops.rs` ROOM_JOIN/LEAVE/cleanup 3곳 Vec 처리 + floor.queue 정보
- `ingress.rs` MBCP Vec 처리 + send_ws_floor_taken priority 필드

---

## [0.6.2] - 2026-03-20

### Refactored (리팩토링 2차: 공통 헬퍼 추출 + lib.rs 분리)

#### PLI burst 공통 헬퍼 — transport/udp/pli.rs (신규)
- `spawn_pli_burst()` 함수 추출: 5곳 중복 PLI burst 코드 통합
  - track_ops.rs: CAMERA_READY [0,150], SUBSCRIBE_LAYER [0,200,500,1500]
  - floor_ops.rs: FLOOR_REQUEST [0,500,1500]
  - ingress.rs: fanout_simulcast_video [0,200,500,1500], send_pli_burst [0,500,1500]
- delays 배열 + log_prefix 파라미터화로 호출처별 차이 유지

#### Subscribe tracks 수집 헬퍼 — handler/helpers.rs
- `collect_subscribe_tracks()` 추출: 3곳 동일 로직 통합
  - room_ops.rs: handle_room_join (existing_tracks)
  - room_ops.rs: handle_room_sync (subscribe_tracks)
  - track_ops.rs: handle_tracks_ack (resync_tracks)

#### PTT silence flush 헬퍼 — handler/helpers.rs
- `flush_ptt_silence()` 추출: 3곳 동일 로직 통합
  - floor_ops.rs: handle_floor_release
  - room_ops.rs: handle_room_leave, cleanup

#### Remove tracks 빌드 헬퍼 — handler/helpers.rs
- `build_remove_tracks()` 추출: 3곳 동일 로직 통합
  - room_ops.rs: handle_room_leave, cleanup
  - tasks.rs: run_zombie_reaper

#### cleanup() 구조 개선 — handler/room_ops.rs
- state.rooms.get() 4회 반복 → 1회로 통합 (Arc clone 재사용)
- 단계별 주석으로 가독성 개선

#### lib.rs 분리
- `tasks.rs` (신규) — run_floor_timer + run_zombie_reaper 이동
- `startup.rs` (신규) — load_env_file + env_or + detect_local_ip + create_default_rooms 이동
- `broadcast_to_room_all` 제거 → handler/helpers::broadcast_to_room 통합
- lib.rs: 491줄 → ~190줄

## [0.6.1] - 2026-03-20

### Refactored (리팩토링 1차: handler 분할 + ingress 함수 분해)

#### signaling/handler.rs → signaling/handler/ 디렉토리 분할
- `handler.rs` 1,753줄 단일 파일 → 7개 파일로 분할 (최대 548줄)
- `mod.rs` (211줄) — Session, WS entry point, dispatch
- `room_ops.rs` (548줄) — IDENTIFY, ROOM_LIST/CREATE/JOIN/LEAVE/SYNC, MESSAGE, cleanup
- `track_ops.rs` (547줄) — PUBLISH_TRACKS, TRACKS_ACK, MUTE_UPDATE, CAMERA_READY, SUBSCRIBE_LAYER
- `floor_ops.rs` (249줄) — FLOOR_REQUEST/RELEASE/PING, apply_floor_action
- `helpers.rs` (138줄) — broadcast_to_others/room, codec/extmap policy, simulcast SSRC, utilities
- `admin.rs` (128줄) — admin WS handler, build_rooms_snapshot
- `telemetry.rs` (32줄) — client telemetry passthrough

#### transport/udp/ingress.rs — handle_srtp 함수 분해
- `handle_srtp` 400줄 → 124줄 (3.2배 축소)
- `process_publish_rtcp()` (67줄) 추출 — SRTCP decrypt + MBCP + SR consume + relay
- `collect_rtp_stats()` (68줄) 추출 — audio jitter, recv_stats, RTP cache, TWCC
- `prepare_fanout_payload()` (73줄) 추출 — PTT gating + SSRC rewrite
- `fanout_simulcast_video()` (100줄) 추출 — simulcast 레이어 라우팅 + PLI burst

### 동작 변경 없음 — 순수 구조 리팩토링

## [0.6.0] - 2026-03-20

### Added (Simulcast Phase 3: 가상 SSRC + SimulcastRewriter + 레이어 전환)

#### participant.rs
- `SimulcastRewriter` 구조체 — SSRC/seq/ts rewrite, 키프레임 대기, 레이어 전환 offset 재계산
- `SubscribeLayerEntry` 구조체 — subscriber별 publisher 레이어 구독 상태
- `simulcast_video_ssrc: AtomicU32` — publisher별 고정 가상 video SSRC (CAS lazy 할당)
- `subscribe_layers: Mutex<HashMap>` — subscriber별 레이어 상태
- `ensure_simulcast_video_ssrc()` — CAS 기반 lazy 할당

#### room.rs
- `find_publisher_by_vssrc()` — 가상 video SSRC로 publisher 찾기 (PLI/NACK 역매핑)

#### message.rs
- `SubscribeLayerRequest`, `SubscribeLayerTarget` 구조체

#### handler.rs
- `handle_subscribe_layer()` (op=51) — 레이어 선택 + PLI 교착 방지 대책 #2
- `simulcast_replace_video_ssrc()` — TRACKS_UPDATE/ROOM_JOIN/ROOM_SYNC에서 가상 SSRC 교체
- `simulcast_replace_video_ssrc_direct()` — ROOM_LEAVE/cleanup용
- SSRC 참조 경로 10항목 전체 수정 (TRACKS_UPDATE, ROOM_JOIN, ROOM_SYNC, ROOM_LEAVE, TRACKS_ACK, TRACKS_RESYNC)

#### ingress.rs
- Simulcast video fan-out: SubscribeLayer 확인 + SimulcastRewriter.rewrite()
- PLI 교착 방지 대책 #1: SubscribeLayer 즉석 생성 시 PLI burst [0,200,500,1500]ms
- PLI relay: 가상 SSRC → 실제 SSRC 역매핑 (find_publisher_by_vssrc)
- NACK: 가상 SSRC/seq → 실제 SSRC/seq 역매핑 (reverse_seq)
- SR translation: simulcast video는 가상 SSRC로 send_stats 조회

### Fixed
- TWCC extmap ID 동적 참조 — client-offer 모드에서 Chrome 할당 ID 사용 (0이면 서버 기본값 fallback)
  기존: 하드코딩 ID=6 → Chrome 할당 ID=3 불일치 → TWCC 추출 실패 → BWE 30kbps 추락

## [0.5.9] - 2026-03-20

### Added (분석 다각화: PLI/NACK/RTX per-participant 계측)

#### participant.rs

- PipelineStats 3종 추가: `pub_pli_received`, `sub_nack_sent`, `sub_rtx_received`

#### ingress.rs

- subscriber NACK 수신 → `sub_nack_sent` 계측
- PLI subscriber relay → `pub_pli_received` + agg `pli_subscriber_relay`
- RTX 전송 성공 → `sub_rtx_received` 계측

#### egress.rs

- subscribe_ready PLI → `pub_pli_received` + agg `pli_server_initiated`

#### pli.rs

- PLI burst (floor/simulcast/camera) → `pub_pli_received` + agg `pli_server_initiated`

#### admin (oxlens-home)

- `app.js`: PIPELINE_FIELDS에 3종 추가
- `snapshot.js`: pub/sub 라인에 pli/nack/rtx, trend에 pub_pli/sub_nack 추가

## [0.5.8] - 2026-03-20

### Added (AggLogger + Room Active Session)

#### agg_logger.rs (신규)

- `AggLogger` — OnceLock 싱글톤, DashMap<u64, AggEntry> 해시키 기반 집계
- `agg_key()` — 호출자가 키 재료로 해시 생성
- `inc()` / `inc_with()` / `register()` — 핫패스 atomic 카운터
- `clear_room()` — room_id 기반 엔트리 일괄 제거
- `flush()` → TelemetryBus emit(AggLog) 자동 호출
- TelemetryEvent::AggLog variant 활성화

#### room.rs

- `Room.active_since: AtomicU64` 추가 (0=비활성)
- `add_participant()`: `compare_exchange(0, now)` — 첫 join 시 활성 세션 시작

#### ingress.rs

- 6개 핫패스 반복 warn → `agg_logger::inc_with()` 교체
  - egress_queue_full, audio_gap, nack_pub_not_found, nack_no_rtx_ssrc, rtx_cache_miss, rtx_budget_exceeded
- 기존 수동 rate-limit (`if metrics.load()==0 { warn!() }`) 제거
- GlobalMetrics 카운터는 그대로 유지

#### transport/udp/mod.rs

- `flush_metrics()`: 빈 방 감지 → `active_since` 리셋 + `clear_room()`
- pipeline JSON에 `_active_since` 포함
- `agg_logger::flush()` 호출 추가

#### admin (oxlens-home)

- `app.js`: `_active_since` 캡처 + 메타 필드 스킵
- `snapshot.js`: `room_active` 표시

## [0.5.7] - 2026-03-20

### Refactored (TelemetryBus: 수집/전송 책임 분리)

#### telemetry_bus.rs (신규)

- `TelemetryBus` — OnceLock 전역 싱글톤 + mpsc→broadcast 버스 태스크
- `TelemetryEvent` enum: ClientTelemetry, ServerMetrics, RoomSnapshot (확장 용이)
- `emit()` — 어디서든 호출 가능, 의존성 zero. try_send로 핫패스 안 막힘
- `subscribe()` — 어드민 WS 구독용

#### state.rs

- `admin_tx: broadcast::Sender<String>` 필드 제거

#### 리와이어링 (동작 변경 없음)

- `telemetry.rs`: `state.admin_tx.send()` → `telemetry_bus::emit(ClientTelemetry)`
- `admin.rs`: `state.admin_tx.subscribe()` → `telemetry_bus::subscribe()`
- `helpers.rs`: `state.admin_tx.send()` → `telemetry_bus::emit(RoomSnapshot)`
- `transport/udp/mod.rs`: `self.admin_tx` 필드/인자 전부 제거, `emit(ServerMetrics)` 사용
- `lib.rs`: `telemetry_bus::init()` 초기화 추가, UdpTransport 생성에서 admin_tx 제거

## [0.5.6] - 2026-03-20

### Added (Pipeline Stats: per-participant 파이프라인 카운터 — AI 진단 기반)

#### participant.rs

- `PipelineStats` 구조체 추가 — publisher/subscriber 관점 파이프라인 통과량 카운터 (7종)
  - pub: `pub_rtp_in`, `pub_rtp_gated`, `pub_rtp_rewritten`, `pub_video_pending`
  - sub: `sub_rtp_relayed`, `sub_rtp_dropped`, `sub_sr_relayed`
- `PipelineSnapshot` + `to_json()` — flush 시 누적값 스냅샷 (counter 타입, swap 안 함)
- `Participant.pipeline: PipelineStats` 필드 추가

#### ingress.rs

- 핫패스 6개 지점에 participant별 `fetch_add(1, Relaxed)` 계측 추가
  - ingress 수신, PTT gate, PTT rewrite, PTT pending drop, egress relay, egress drop, SR relay

#### transport/udp/mod.rs

- `flush_metrics()`: room별→participant별 pipeline snapshot 수집 → `"pipeline"` 필드로 admin JSON에 포함
- 각 participant에 `since` (joined_at) 첨부 — counter 누적 기준점

#### admin (oxlens-home)

- `state.js`: `pipelineRing` (Map) — per-participant delta 계산용 링버퍼
- `app.js`: `processPipeline()` — counter 누적값에서 delta 계산 + 20슬롯 링버퍼 push
- `snapshot.js`: `--- PIPELINE STATS ---` 섹션 추가 — total(+delta) + trend 출력

## [0.5.5] - 2026-03-13

### Added (Phase TV-2: 델타 기반 손실 분석 + 스냅샷 타임스탬프 + 세션 추적)

#### handler.rs

- `build_rooms_snapshot()`: participant에 `joined_at` (입장 시각 ms) 필드 추가
- `build_rooms_snapshot()`: room에 `created_at` (방 생성 시각 ms) 필드 추가
- snapshot 루트에 `"ts": current_ts()` 추가

#### metrics/mod.rs

- `flush()` 최종 JSON에 `"ts": SystemTime millis` 추가

### Changed

- (서버 코드 변경은 위 2개 파일만 — 나머지는 클라이언트/어드민 측 변경)

## [0.5.4] - 2026-03-11

### Added (Phase T-6: 진단 공백 해소 — 인코더 심층 진단 + 구간별 손실 + 이벤트 타임라인)

클라이언트 텔레메트리 수집 확장 + 어드민 시각화. 서버 코드 변경 없음 (passthrough).

#### 클라이언트 SDK (common/telemetry.js)

**구간 A 수집 항목 확장 (outbound-rtp):**

- `framesSent` — 전송 프레임 수. `framesEncoded - framesSent` 갭으로 인코더 병목 감지
- `hugeFramesSent` — 큰 프레임(키프레임) 전송 수. PLI 응답 후 burst 패턴 감지
- `totalEncodeTime` — 누적 인코딩 시간. `÷ framesEncoded × 1000`으로 프레임당 ms 역산
- `qualityLimitationDurations` 3초 delta — `_prevStats.qld` Map에 이전값 저장, diff 계산. bandwidth/cpu/none/other 각 초 단위

**이벤트 타임라인 (10종 상태 전이 감지):**

- `_eventLog` 링버퍼 (최대 50개) + `_watchState` (SSRC별 이전 감시값)
- `_detectPublishEvents()` — quality_limit_change, encoder_impl_change, pli_burst(≥3), nack_burst(≥10), bitrate_drop(50%↓), fps_zero
- `_detectSubscribeEvents()` — video_freeze, loss_burst(≥20), frames_dropped_burst(≥5), decoder_impl_change, fps_zero
- `_pendingEvents` → stats 보고에 `events[]` 배열로 포함
- `getEventLog()` — 전체 이벤트 로그 반환 (스냅샷/디버그용)

#### 어드민 (admin/app.js)

**참가자 상세 패널 확장 (renderDetail):**

- Publish video에 3번째 줄 추가: enc-sent gap, huge, enc time, qld delta 표시
- 색상 규칙: gap>5 빨강, gap>0 노랑, enc>30ms 빨강, enc>16ms 노랑, bw 노랑, cpu 빨강

**구간별 손실 Cross-Reference (renderLossCrossRef):**

- pub.packetsSent vs sub.(packetsReceived + packetsLost) 매칭
- `A→B:~N` (추정 전송 중 손실) + `B→C:N` (sub 확정 손실) 표시
- 참가자 상세 패널 하단 "구간별 손실 추정" 섹션

**이벤트 타임라인 표시 (renderEventTimeline):**

- `eventHistory` Map (user_id → 최근 50개) + `handleClientTelemetry`에서 누적
- 최신순 표시, 아이콘(⚡🔧🔑📦📉⏸🧊💀🗑) + 색상 + 시간 + 설명
- 참가자 상세 패널 하단 "이벤트 타임라인" 섹션 (max-h-48 스크롤)

**Contract 체크 추가:**

- `encoder_bottleneck` — `framesEncoded - framesSent` gap ≤ 5 판정

**스냅샷 확장:**

- `--- PUBLISH ---` 섹션: video 인코더 진단 라인 추가 (`[user:video:enc] encoded=N sent=N gap=N huge=N enc_time=Xms/f qld_delta=[bw=Xs cpu=Xs]`)
- `--- LOSS CROSS-REFERENCE ---` 섹션 신규 (`[pub→sub:kind] pub_sent=N sub_recv=N sub_lost=N transit_loss≈N`)
- `--- EVENT TIMELINE ---` 섹션 신규 (전체 이벤트 로그, ISO 타임스탬프)

#### 문서

- `TELEMETRY.md` — 구간 A 지표 테이블 확장, §7 구간별 손실 Cross-Reference 신규, §8 이벤트 타임라인 신규, Contract 12항목으로 확장, 스냅샷 포맷 업데이트, 현재 상태 v0.5.4 반영

## [0.5.3] - 2026-03-09

### Added (Phase M-1: MBCP Floor Control over UDP — RTCP APP 패킷)

네이티브 클라이언트를 위한 UDP 경유 Floor Control. 기존 WS 시그널링 Floor Control과 병렬 운영.

#### config.rs

- RTCP APP 상수 7개 추가: `RTCP_PT_APP`(204), `MBCP_SUBTYPE_FREQ`~`MBCP_SUBTYPE_FPNG`(0~5), `MBCP_APP_NAME`("MBCP")

#### rtcp.rs — MBCP APP 파서/빌더

- `MbcpMessage` 구조체 — 파싱된 MBCP 메시지 (subtype, ssrc, data)
- `parse_mbcp_app()` — RTCP APP 패킷에서 MBCP 메시지 추출 (name="MBCP" 검증, zero-padding 제거)
- `build_mbcp_app()` — 서버→클라이언트 MBCP APP 패킷 빌더 (4바이트 경계 패딩)
- `split_compound_rtcp()` — PT=204 APP 분류 추가 → `mbcp_blocks` 필드
- 유닛테스트 7개: build/parse 라운드트립, 4바이트 정렬, non-MBCP 거부, compound 분류

#### ingress.rs — MBCP 수신 처리

- `handle_mbcp_from_publish()` — Publish RTCP compound 내 MBCP APP 처리
  - FREQ → `floor.request()` + rewriter 전환 + PLI burst
  - FREL → `floor.release()` + rewriter 정리
  - FPNG → `floor.ping()` (단방향 heartbeat)
- `apply_mbcp_floor_action()` — FloorAction → MBCP APP 패킷 브로드캐스트
  - Granted → FTKN broadcast (subscriber egress 큐)
  - Denied → FRVK to requester only
  - Released → FIDL broadcast
  - Revoked → FRVK to prev_speaker + FIDL broadcast
- `broadcast_mbcp_to_subscribers()` — room 내 모든 subscriber egress 큐에 RTCP APP 전달
- `send_mbcp_to_participant()` — 특정 participant에게 RTCP APP 전달
- WS 호환 이벤트 동시 전송: `send_ws_floor_taken()`, `send_ws_floor_idle()`
- `send_pli_burst()` — Floor Granted 시 PLI 3연발 (0ms/500ms/1500ms)
- Subscribe RTCP 로그에 mbcp_blocks 카운트 추가

#### 하이브리드 설계

- 웹 클라이언트: 기존 WS Floor Control (op 40~42, 141~143) 그대로 유지
- 네이티브 클라이언트: SRTP 채널로 RTCP APP 패킷 송수신 (별도 포트/암호화 불필요)
- Floor Granted/Idle 시 WS + UDP 양쪽 모두 이벤트 발행 → 혼합 환경 지원

### Refactored

- `handle_subscribe_rtcp()` — RTCP relay 로직을 `relay_subscribe_rtcp_blocks()`로 추출 (가독성)

## [0.5.2] - 2026-03-08

### Added (Phase E-5: PTT 클라이언트 Subscribe SDP 연동)

#### 서버 (handler.rs)

- ROOM_JOIN 응답에 `ptt_virtual_ssrc` 필드 추가 (PTT 모드에서만)
  - `{ audio: u32, video: u32 }` — subscribe SDP에 원본 SSRC 대신 선언할 가상 SSRC
- Floor Granted 시 PLI 3회 반복 전송 (0ms, 500ms, 1500ms)
  - hard_off 복귀 시 getUserMedia + replaceTrack 비동기 지연 커버
  - tokio::spawn 비동기 타이머로 독립 실행

#### 서버 (metrics/mod.rs, ingress.rs)

- PTT 메트릭 5개 추가: `audio_rewritten`, `video_rewritten`, `video_skip`, `floor_released`, `speaker_switches`
- ingress.rs: RewriteResult 분기에서 audio/video 분리 카운팅 + video Skip 카운팅

#### 클라이언트 (sdp-builder.js)

- `buildPttSubscribeSdp()` 신규 — 가상 SSRC 1쌍(audio+video)으로 2개 m-line only
- `buildSubscribeRemoteSdp()` 3번째 파라미터 `options` 추가 (mode, pttVirtualSsrc)
- PTT 분기를 empty tracks early return보다 앞으로 이동
- `updateSubscribeRemoteSdp` options 패스스루

#### 클라이언트 (livechat-sdk.js, media-session.js)

- `_onJoinOk`: ROOM_JOIN 응답의 mode/ptt_virtual_ssrc를 MediaSession에 전달
- `MediaSession.setup`: `_sdpOptions` 저장, PTT subscribe PC 생성 우회
- `_setupSubscribePc`: PTT 모드일 때 트랙 없어도 subscribe PC 생성

#### 클라이언트 UI (app.js)

- PTT 가상 스트림 `stream.id === "light-ptt"` 감지 → `"__ptt__"` 키로 저장
- PTT video track `onunmute`/`onmute` 이벤트 핸들러
- `updatePttView("listening")`: `remoteStreams.get("__ptt__")` 사용
- `updatePttView("idle")`: PTT 모드에서 `video.srcObject = null` 안 함
- listening 상태에서 `video.play()` 강제 호출

#### 어드민 (livechat-admin/app.js)

- SFU 패널 PTT 메트릭: audio_rw, video_rw, vid_skip, released, switches 추가
- 스냅샷 텍스트에 새 메트릭 포함

### Fixed

- VP8 키프레임 감지(`is_vp8_keyframe()`) 정상 동작 확인 — `kf_arrived=0`은 텔레메트리 3초 윈도우 타이밍 이슈

### Known Issues (WebRTC 기반 PTT 한계)

- 화자 교대 시 0.5-2초 영상 정지 (키프레임 대기)
- hard_off 복귀 시 검은 화면 가능 (PLI 타이밍 불일치)
- idle 구간에서 Chrome track muted 전이 → unmute 지연
- 오디오 지연 관찰 → 추가 분석 필요

## [0.5.1] - 2026-03-08

### Added (Phase E: PTT 미디어 파이프라인 — 게이팅 + SSRC 리라이팅 + 키프레임 대기)

#### E-0: Floor Timer Task

- `lib.rs` — `run_floor_timer()` 2초 주기 task (T2 max burst 30초 + T_FLOOR_TIMEOUT ping 미수신 5초 감시)
- 타임아웃 시 FLOOR_REVOKE 전송 + FLOOR_IDLE 브로드캐스트 + rewriter 정리

#### E-1: Relay Gate (미디어 게이팅)

- `ingress.rs` — `handle_srtp` fan-out 직전 PTT 모드 가드: floor holder만 통과
- `ingress.rs` — `relay_publish_rtcp` SR relay에도 동일 PTT 가드
- Conference 모드 zero impact (`room.mode != Ptt`면 분기 안 탐)

#### E-2: Audio SSRC Rewriting

- `room/ptt_rewriter.rs` — PttRewriter 구조체 (오프셋 기반 상대 연산, 기술타당성검토서 §2.3)
  - `new_audio()`: ts_guard_gap=960 (Opus 48kHz, 20ms), 키프레임 대기 없음
  - `switch_speaker()` / `clear_speaker()`: 화자 전환 시 오프셋 갱신
  - `rewrite()`: in-place RTP 헤더 덮어쓰기 (SSRC + seq + ts + marker bit)
  - `reverse_seq()`: NACK 역매핑 (가상seq → 원본seq)
- `room/room.rs` — Room에 `audio_rewriter: PttRewriter` 필드

#### E-4: Video SSRC Rewriting + 키프레임 대기

- `room/ptt_rewriter.rs` — `new_video()`: ts_guard_gap=3000 (VP8 90kHz, ~33ms), `pending_keyframe` 상태
  - `RewriteResult` enum: Ok / PendingKeyframe / Skip
  - `is_vp8_keyframe()`: VP8 payload descriptor 파싱으로 I-frame 감지 (RFC 7741)
- `room/room.rs` — Room에 `video_rewriter: PttRewriter` 필드
- `ingress.rs` — 비디오 RTP(PT=96 VP8) 리라이팅 + 키프레임 도착 전 P-frame 드롭
- `ingress.rs` — NACK 역매핑: 가상SSRC NACK → 원본seq 역산 → RtpCache 조회
- `ingress.rs` — subscribe RTCP relay: 가상SSRC → 원본SSRC 변환 (RR/PLI publisher 라우팅)

#### PTT 메트릭 (7개 카운터)

- `metrics/mod.rs` — GlobalMetrics에 PTT 전용 카운터 추가
  - `ptt_rtp_gated`: E-1 gate에서 드롭된 비발화자 RTP 수
  - `ptt_rtp_rewritten`: 리라이팅 성공 RTP 수
  - `ptt_video_pending_drop`: 키프레임 대기 중 드롭된 P-frame 수
  - `ptt_keyframe_arrived`: 화자 전환 후 키프레임 도착 횟수
  - `ptt_floor_granted` / `ptt_floor_revoked`: Floor 상태 전환 횟수
  - `ptt_nack_remapped`: 가상SSRC NACK 역매핑 횟수
- flush JSON에 `"ptt"` 섹션으로 출력

#### PTT 상태 스냅샷 (어드민)

- `handler.rs` — `build_rooms_snapshot()`에 PTT 모드 room 정보 추가
  - `floor_speaker`, `audio_virtual_ssrc`, `video_virtual_ssrc`

### Changed

- `state.rs` — AppState에 `Arc<GlobalMetrics>` 필드 추가 (handler에서 PTT 메트릭 접근용)
  - `metrics` 필드 / `new()` → `pub(crate)` (가시성 정합)
- `lib.rs` — GlobalMetrics 생성을 AppState 생성 전으로 이동 (metrics 전달 순서)
- `signaling/handler.rs` — 5곳에서 audio_rewriter + video_rewriter 화자 전환/정리 호출
  - handle_floor_request (Granted → switch_speaker)
  - handle_floor_release (Released → clear_speaker)
  - handle_room_leave (auto-release → clear_speaker)
  - cleanup/disconnect (auto-release → clear_speaker)
  - run_floor_timer (Revoked → clear_speaker)

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

- (이하 동일, 생략)
