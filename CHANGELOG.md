# Changelog

All notable changes to this project will be documented in this file.

> **명칭 변경**: v0.5.2까지는 `light-livechat` / `livechatd`로 개발.
> v0.5.3부터 `oxlens-sfu-server` / `oxsfud`로 현행화.

Format follows [Keep a Changelog](https://keepachangelog.com/).

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
