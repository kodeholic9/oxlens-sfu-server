# TODO — Implementation Roadmap

## Phase 0: STUN / ICE-Lite ✅
- [x] STUN message parsing (hand-rolled, RFC 8489)
- [x] STUN binding response generation (XOR-MAPPED-ADDRESS, MESSAGE-INTEGRITY, FINGERPRINT)
- [x] ICE credential verification (ufrag:pwd matching)
- [x] USE-CANDIDATE handling
- [x] Single UDP socket listener with demux dispatch
- [x] Unit tests for STUN parsing

## Phase 1: DTLS + SRTP Modules ✅
- [x] Server certificate generation (self-signed, per-instance)
- [x] SHA-256 fingerprint (for SDP answer)
- [x] DemuxConn adapter (Conn trait → mpsc channel bridge)
- [x] DTLS passive handshake function (dtls 0.17.1)
- [x] SRTP key derivation layout (RFC 5764 §4.2, 60 bytes)
- [x] SRTP context: encrypt_rtp, decrypt_rtp, decrypt_rtcp
- [x] SRTP roundtrip unit test
- [x] ServerCert in AppState

## Phase 1.5: Room + Participant + Signaling ✅
- [x] Participant: ICE ufrag/pwd, latched address, SRTP contexts, tracks
- [x] Room: 3-index DashMap (user_id, ufrag, addr) — all O(1)
- [x] RoomHub: reverse indices (ufrag → room_id, addr → room_id)
- [x] STUN latch with NAT rebinding support
- [x] IDENTIFY handler (token → user_id)
- [x] ROOM_CREATE handler
- [x] ROOM_JOIN handler (ICE credentials, DTLS fingerprint, member list)
- [x] ROOM_LEAVE handler + disconnect cleanup
- [x] Participant join/leave broadcast (ROOM_EVENT)
- [x] MESSAGE relay (MESSAGE_EVENT)
- [x] ICE_CANDIDATE (trickle — acknowledged, ignored in ICE-Lite)

## Phase 2: UDP ↔ RoomHub Integration ✅
- [x] STUN handler uses RoomHub.latch_by_ufrag() (per-participant ICE credentials)
- [x] MESSAGE-INTEGRITY verification with participant's ice_pwd
- [x] USE-CANDIDATE → trigger DTLS handshake (10s timeout, async spawn)
- [x] DTLS handshake complete → export_srtp_keys() → install on Participant
- [x] SRTP hot path: find_by_addr() O(1) → decrypt → fan-out → encrypt → send
- [x] RTCP detection (RFC 5761 PT demux), decrypt for logging, no relay
- [x] DemuxConn channel switched to Bytes
- [x] DTLSConn keepalive recv loop
- [x] Stale DTLS session cleanup
- [x] AppState shared between WS handlers and UDP transport

## Phase 3: SDP Negotiation ✅ (v0.1.4)
- [x] SDP Offer parsing + Answer generation
- [x] Browser integration test (3-person grid)
- [x] Debug logging + video rendering fix

## Phase A-1: 2PC / SDP-free Architecture ✅ (v0.1.5)
- [x] PcType enum (Publish / Subscribe)
- [x] MediaSession 구조체 (ufrag, ice_pwd, address, srtp per session)
- [x] Participant: publish + subscribe MediaSession 소유
- [x] Room: by_ufrag/by_addr → (Participant, PcType) 매핑
- [x] RoomHub: 참가자당 2개 ufrag 역인덱스
- [x] Signaling: server_config JSON 응답 (SDP 제거)
- [x] PUBLISH_TRACKS 핸들러 (클라이언트 트랙 SSRC 등록)
- [x] TRACKS_UPDATE 이벤트 (subscribe re-nego 트리거)
- [x] UDP: PcType 식별, publish 수신 → subscribe 전송
- [x] transport/sdp.rs 삭제
- [x] Router 제거 (sockaddr 기반 직접 릴레이)

## Phase A-2: 클라이언트 SdpBuilder ✅ (v0.1.6)
- [x] SdpBuilder JS 모듈 개발 (sdp-builder.mjs)
- [x] SdpBuilder 유닛테스트 74개 전부 통과
- [x] livechat-sdk.js v2.0.0 (2PC/SDP-free 전환)

## Phase A-3: PLI 키프레임 요청 ✅ (v0.1.7)
- [x] RTCP PLI 패킷 생성 함수 (12바이트 고정, RFC 4585)
- [x] SrtpContext.encrypt_rtcp() 추가
- [x] subscribe SRTP ready 이벤트 시점에 PLI 트리거

## Phase C: RTCP + 안정화 ✅
- [x] RTCP SR/RR transparent relay — v0.2.2
- [x] NACK 수신 → 서버에서 RTX 재전송 — v0.2.0
- [x] REMB + PLI relay — v0.2.2
- [x] mute/unmute 이벤트 처리 — v0.2.3
- [x] TWCC feedback 생성 — v0.3.8

## Phase D: Hardening
- [ ] IDENTIFY token verification (JWT or shared secret)
- [x] Zombie session reaper — v0.2.1
- [x] Heartbeat timeout → disconnect — v0.2.1
- [x] Graceful shutdown — v0.2.1

## Phase E: PTT Support ✅
- [x] Floor control state machine — v0.5.0
- [x] E-0~E-5: 게이팅 + SSRC 리라이팅 + 키프레임 + 클라이언트 연동 — v0.5.1~v0.5.2

## Phase M: MBCP Floor Control over UDP ✅ (v0.5.3)
- [x] M-1: RTCP APP 패킷 파서/빌더 (RFC 3550 Section 6.7) — v0.5.3
- [x] M-1: ingress MBCP 수신 → floor.rs 상태 머신 연결 — v0.5.3
- [x] M-1: MBCP 응답 브로드캐스트 (FTKN/FIDL/FRVK → subscriber egress) — v0.5.3
- [x] M-1: WS + UDP 하이브리드 Floor 이벤트 동시 발행 — v0.5.3
- [x] M-1: 유닛테스트 7개 (build/parse 라운드트립, 4바이트 정렬, compound 분류) — v0.5.3

## Phase W: UDP Worker 멀티코어 분산 ✅
- [x] W-1: Fan-out spawn — v0.3.5
- [x] W-2: Multi-worker (SO_REUSEPORT) — v0.3.6
- [x] W-3: Subscriber Egress Task — v0.3.7

---

## 🔜 Next: oxlens-sdk-core (Android 네이티브 SDK)

### 환경 구축
- [x] WSL2 + Ubuntu 22.04 설치 — 2026-03-09
- [x] depot_tools 설치 — 2026-03-09
- [x] WebRTC Android 소스 fetch + gclient sync — 2026-03-09
- [x] install-build-deps 완료 — 2026-03-09
- [ ] libwebrtc arm64 첫 빌드 완료 (gn gen + ninja -j4)
- [ ] libwebrtc.aar 생성 확인
- [ ] Android Studio 설치 + SDK/NDK 셋업
- [ ] oxlens-sdk-core 프로젝트 생성 (Kotlin, min API 28)
- [ ] libwebrtc.aar 통합 + Gradle sync 확인

### Rx 파이프라인 (수신)
- [ ] C++ Interceptor: Dependencies 주입용 Proxy 클래스 작성
- [ ] Opus silence frame 주입기 (NetEQ Hot 유지)
- [ ] Sequence/Timestamp offset 보정 (서버 릴레이 로직 포팅)
- [ ] SSRC 기반 Audio/Video 패킷 분류

### Tx 파이프라인 (송신 FSM)
- [ ] Stage 1 (Soft-Mute): `encoding.active = false`, 하드웨어 ON
- [ ] Stage 2 (Hard-Mute): 하드웨어 OFF, Track 유지, "비디오 중단" 시그널
- [ ] Stage 3 (Deep Sleep): ICE Ping 장주기 전환
- [ ] Stage 전환 타임아웃 config 상수화 (1분, 10분)

### ICE Keep-alive 최적화
- [ ] `ice_candidate_pair_ping_interval` 동적 조정 (Stage별 10s/15s/30s+)
- [ ] 한국 통신사(SKT/KT/LGU+) LTE/5G NAT 타임아웃 실측

### 카메라 웜업 & 키프레임
- [ ] Android `onFirstFrameAvailable()` 콜백 연동
- [ ] 클라이언트 → 서버: CAMERA_READY 시그널 (opcode 추가)
- [ ] 서버: CAMERA_READY 수신 → PLI 2발 (즉시 + 150ms)

### MBCP 클라이언트 (네이티브 Floor Control)
- [ ] RTCP APP 패킷 빌더 (FREQ/FREL/FPNG) — C++ 또는 Kotlin/JNI
- [ ] RTCP APP 패킷 파서 (FTKN/FIDL/FRVK) — subscribe PC에서 수신
- [ ] PTT 버튼 UI → MBCP FREQ/FREL 전송
- [ ] sfu-labs bench 클라이언트에 MBCP 송수신 추가 (서버 검증용)

### 서버 추가 작업
- [ ] opcode 추가: CAMERA_READY, VIDEO_SUSPENDED, VIDEO_RESUMED
- [ ] VIDEO_SUSPENDED/RESUMED 핸들러 → 수신자 UI placeholder 전환 브로드캐스트
- [ ] 시그널링 단절 시 Full Cold Start 정책 구현

### 시그널링 단절 예외 처리
- [ ] 시그널링(TCP) 단절 감지 → 모든 캐시(SDP/DTLS/ICE) 파기
- [ ] Full Cold Start (신규 SDP 교환) 수행
- [ ] 좀비 세션 원천 차단 검증

---

## Backlog
- [ ] IDENTIFY token verification (JWT or shared secret)
- [ ] VP8 키프레임 캐시 (서버, 화자 전환 시 즉시 주입)
- [ ] PTT 오디오 지연 원인 분석 및 최적화
- [ ] x86 서버 벤치마크 (50인+ 목표)
- [ ] Simulcast / SVC
- [ ] TURN relay support
- [ ] Recording (RTP → file)
- [ ] Data channel support
- [ ] Horizontal scaling (multi-node)
- [ ] 타우리 데스크톱: WebView 내장 WebRTC vs 네이티브 libwebrtc 결정
