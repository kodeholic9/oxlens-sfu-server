# OxLens SFU Server

> author: kodeholic (powered by Claude)

경량 Conference SFU 서버 + PTT 확장. B2B 타겟 (파견센터, 보안, 물류, 발전소).

**2PC / SDP-free architecture** — 서버는 SDP를 모르고, 정책 JSON만 통보한다.

## Architecture

- **2PC**: 참가자당 PeerConnection 2개 (publish + subscribe 분리)
- **SDP-free**: 서버는 SDP를 파싱/조립하지 않음. 코덱/PT/extmap은 서버 정책으로 고정
- **Bundle**: 모든 미디어가 단일 UDP 포트로 멀티플렉싱
- **ICE-Lite**: 서버는 passive controlled agent (STUN binding request 미전송)

```
Browser / Native SDK (WebRTC)
    ├── WebSocket ──→ [Axum WS] ── Signaling (server_config JSON, tracks_update)
    ├── publish PC ──→ [Single Port] ── 내 미디어 → 서버 (recvonly)
    └── subscribe PC ─→ [Single Port] ── 다른 참가자 미디어 ← 서버 (sendonly)
                          ├── ICE-Lite  (STUN binding response, PC별 ufrag 식별)
                          ├── DTLS 1.2  (handshake → SRTP key derivation, PC별 독립)
                          └── SRTP      (decrypt → route → encrypt)
```

### 핵심 원칙

1. **서버는 SDP를 모른다** — SDP는 WebRTC의 사정이지 RTP의 사정이 아니다
2. **능력치 교환은 JSON 기반** — 코덱, PT, extmap, RTCP feedback을 구조화된 JSON으로 교환
3. **SDP 조립은 클라이언트 책임** — 서버 정책 JSON → fake remote SDP → setRemoteDescription
4. **코덱/PT/extmap은 서버 정책으로 고정** — 협상이 아니라 통보

## Features

### Conference 모드
- 양방향 오디오/비디오 (Opus + VP8)
- NACK → RTX 재전송 (서버 RTP 캐시 기반)
- TWCC / BWE 대역폭 추정
- Simulcast Phase 3 (가상 SSRC + SimulcastRewriter + 레이어 전환 + PLI 교착 방지)

### PTT 모드 (MCPTT/MBCP 기반)
- Floor Control 상태 머신 (IDLE/TAKEN, T2 burst 30초 + ping timeout 5초)
- 미디어 게이팅 (비발화자 RTP 드롭)
- SSRC 리라이팅 (오디오 + 비디오 오프셋 기반)
- VP8 키프레임 대기 + PLI burst (0ms/500ms/1500ms)
- MBCP over UDP (RTCP APP PT=204) + WS 시그널링 하이브리드

### RTCP Terminator
- SFU가 두 독립 RTP 세션의 종단점 (relay가 아님)
- Ingress: 서버가 RR 자체 생성 (RFC 3550 A.3/A.8)
- Egress: Publisher SR 기반 변환 릴레이 (서버 클록 SR 생성 금지)
- Subscriber RR → publisher 릴레이 차단
- RTX(PT=97) 수신 통계 제외

### SR Translation
- Conference: SSRC/NTP/RTP ts 원본 유지, packet_count/octet_count만 egress 기준
- PTT: SSRC→가상, RTP ts→오프셋 변환, PTT 모드 SR 릴레이 중단 (jb_delay 방지)
- Simulcast: real video SSRC → virtual SSRC 변환

### Telemetry & Admin
- 클라이언트 telemetry 수집 (Publish/Subscribe delta, 10종 이벤트 감지)
- 어드민 대시보드 (6파일 ES module, 20개 ring buffer, 통합 타임라인)
- GlobalMetrics (AtomicU64 카운터 + 3초 flush)
- PipelineStats (per-participant 파이프라인 카운터)
- Contract 체크 12항목 (rr_generated, rr_consumed, sr_relay, egress_drop 등)
- 텍스트 스냅샷 export (AI 분석용)

## Lookup Architecture

Three O(1) lookup paths — 2PC 구조에서 (Participant, PcType)으로 식별:

```
[Signaling]  room_id + user_id  →  Participant                       (WS handler)
[STUN]       ufrag              →  room_id  →  (Participant, PcType)  (ICE cold path)
[SRTP]       SocketAddr         →  room_id  →  (Participant, PcType)  (media hot path)
```

```
RoomHub
  ├── rooms:       DashMap<room_id, Room>
  ├── ufrag_index: DashMap<ufrag, room_id>       ← STUN reverse index (pub/sub 각각)
  └── addr_index:  DashMap<addr, room_id>        ← SRTP reverse index (pub/sub 각각)

Room
  ├── participants: DashMap<user_id, Participant>          ← primary
  ├── by_ufrag:     DashMap<ufrag, (Participant, PcType)>  ← STUN cold path
  └── by_addr:      DashMap<addr, (Participant, PcType)>   ← SRTP hot path
```

## Signaling Protocol

Packet format:
```json
{ "op": 11, "pid": 42, "d": { ... } }
{ "op": 11, "pid": 42, "ok": true, "d": { ... } }
{ "op": 11, "pid": 42, "ok": false, "d": { "code": 2001, "msg": "room not found" } }
```

### Client → Server

| op | Name | Description |
|----|------|-------------|
| 1 | HEARTBEAT | 생존 확인 |
| 3 | IDENTIFY | 인증 (extra 필드 지원) |
| 9 | ROOM_LIST | 방 목록 요청 |
| 10 | ROOM_CREATE | 방 생성 (mode, simulcast, extra 지원) |
| 11 | ROOM_JOIN | 방 입장 → server_config 수신 |
| 12 | ROOM_LEAVE | 방 퇴장 |
| 15 | PUBLISH_TRACKS | 트랙 SSRC 등록 (rid/twcc_extmap_id 포함) |
| 16 | TRACKS_ACK | subscribe SSRC 확인 응답 → 불일치 시 TRACKS_RESYNC |
| 17 | MUTE_UPDATE | 트랙 mute/unmute → VIDEO_SUSPENDED/RESUMED |
| 18 | CAMERA_READY | 카메라 웜업 완료 → PLI + VIDEO_RESUMED |
| 20 | MESSAGE | 텍스트 메시지 |
| 30 | TELEMETRY | 클라이언트 telemetry 보고 (어드민 passthrough) |
| 40 | FLOOR_REQUEST | PTT 발화권 요청 |
| 41 | FLOOR_RELEASE | PTT 발화권 해제 |
| 42 | FLOOR_PING | 발화자 생존 확인 |
| 50 | ROOM_SYNC | 참여자+트랙+floor 전체 동기화 (폴링 안전망) |
| 51 | SUBSCRIBE_LAYER | Simulcast 레이어 선택 (h/l/pause) |

### Server → Client (Event)

| op | Name | Description |
|----|------|-------------|
| 0 | HELLO | heartbeat_interval 전달 |
| 100 | ROOM_EVENT | 입장/퇴장 |
| 101 | TRACKS_UPDATE | 트랙 추가/제거 |
| 102 | TRACK_STATE | mute 상태 브로드캐스트 |
| 103 | MESSAGE_EVENT | 메시지 브로드캐스트 |
| 104 | VIDEO_SUSPENDED | 비디오 중단 → avatar 전환 |
| 105 | VIDEO_RESUMED | 비디오 재개 |
| 106 | TRACKS_RESYNC | 트랙 목록 재동기화 |
| 110 | ADMIN_TELEMETRY | 어드민 telemetry 중계 |
| 141 | FLOOR_TAKEN | 발화권 획득 브로드캐스트 |
| 142 | FLOOR_IDLE | 발화권 해제 브로드캐스트 |
| 143 | FLOOR_REVOKE | 강제 발화권 회수 |

## Project Structure

```
src/
├── main.rs                     Entry point
├── lib.rs                      Server bootstrap (cert + WS + UDP + background tasks)
├── config.rs                   Global constants (codec, timing, buffer sizes)
├── error.rs                    Error types (1xxx~9xxx)
├── state.rs                    Shared AppState (RoomHub + ServerCert + Metrics)
│
├── signaling/
│   ├── opcode.rs               Opcode constants (Client→Server + Server→Client)
│   ├── message.rs              Packet types & payloads
│   └── handler/
│       ├── mod.rs              Session, WS entry point, dispatch
│       ├── room_ops.rs         IDENTIFY, ROOM CRUD, SYNC, MESSAGE, cleanup
│       ├── track_ops.rs        PUBLISH_TRACKS, TRACKS_ACK, MUTE, CAMERA, SUBSCRIBE_LAYER
│       ├── floor_ops.rs        FLOOR_REQUEST/RELEASE/PING, apply_floor_action
│       ├── helpers.rs          broadcast, codec/extmap policy, simulcast SSRC helpers
│       ├── admin.rs            Admin WS handler, room snapshot builder
│       └── telemetry.rs        Client telemetry passthrough
│
├── transport/
│   ├── demux.rs                Packet classifier (RFC 5764 first-byte)
│   ├── demux_conn.rs           Virtual Conn adapter (mpsc ↔ DTLSConn bridge)
│   ├── stun.rs                 STUN parser/builder (RFC 8489)
│   ├── ice.rs                  ICE-Lite (credential gen)
│   ├── dtls.rs                 DTLS session (cert, fingerprint, handshake, key export)
│   ├── srtp.rs                 SRTP context (encrypt/decrypt, key install)
│   └── udp/
│       ├── mod.rs              UdpTransport (single-port, 2PC demux, STUN, DTLS)
│       ├── ingress.rs          Publish RTP/RTCP 수신 hot path + fan-out
│       ├── egress.rs           Subscriber 방향 송신 (egress task, PLI, REMB, RR)
│       ├── rtcp.rs             RTCP/RTP 파서/빌더 (NACK, SR, PLI, MBCP APP)
│       ├── rtcp_terminator.rs  RecvStats, SendStats, RR builder, SR translation
│       └── twcc.rs             TWCC 대역폭 추정 (recorder, feedback builder)
│
├── media/
│   ├── router.rs               SSRC routing table (reserved)
│   └── track.rs                Track context (reserved)
│
├── room/
│   ├── room.rs                 Room (3-index DashMap O(1)) + RoomHub
│   ├── participant.rs          Participant, MediaSession, Track, SimulcastRewriter, PipelineStats
│   ├── floor.rs                FloorController 상태 머신 (IDLE/TAKEN)
│   └── ptt_rewriter.rs         PttRewriter (SSRC/seq/ts 오프셋 리라이팅)
│
└── metrics/
    ├── mod.rs                  GlobalMetrics (AtomicU64 카운터 + flush JSON)
    ├── env.rs                  EnvironmentMeta
    └── tokio_snapshot.rs       TokioRuntimeSnapshot
```

## Tech Stack

| Component | Technology |
|-----------|-----------|
| Language | Rust (edition 2024) |
| Async Runtime | Tokio |
| HTTP/WS | Axum 0.7 |
| DTLS | dtls 0.17.1 (webrtc-rs) |
| SRTP | webrtc-srtp 0.17.1 |
| STUN | Hand-rolled (RFC 8489) |
| Concurrency | DashMap (segment-locked) |
| Deploy Target | Raspberry Pi (aarch64) / x86 서버 |

## Build & Run

```bash
cargo build --release
RUST_LOG=info cargo run --release

# RPi cross-compile
cargo build --release --target aarch64-unknown-linux-gnu
```

## Design Targets

- Room capacity: 최대 30명 (RPi 기준), 아키텍처는 1000명 검증
- Single UDP port (Bundle), ICE-Lite
- 2PC: publish PC 불변, subscribe PC에서만 re-negotiation
- Conference + PTT 듀얼 모드
- Simulcast (h/l 2-layer, 가상 SSRC rewrite)
- RTCP Terminator (서버 종단, SR translation)
- AI-Native 텔레메트리 설계

## Version

현재 v0.6.1 — 자세한 변경 이력은 [CHANGELOG.md](CHANGELOG.md) 참조.

## Related Projects

| Repository | Description |
|-----------|-------------|
| oxlens-home | Web client + Admin dashboard (Vanilla JS + Tailwind) |
| oxlens-sdk-core | Android Kotlin SDK (libwebrtc AAR 기반) |
| oxlens-sfu-labs | E2E 테스트 + 벤치마크 (Cargo workspace) |
