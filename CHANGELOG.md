# Changelog

All notable changes to this project will be documented in this file.

Format follows [Keep a Changelog](https://keepachangelog.com/).

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
