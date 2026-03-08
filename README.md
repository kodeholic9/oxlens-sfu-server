# light-livechat

High-performance SFU (Selective Forwarding Unit) server built with Rust.
**2PC / SDP-free architecture** — 서버는 SDP를 모르고, 정책 JSON만 통보한다.

## Architecture

- **2PC**: 참가자당 PeerConnection 2개 (publish + subscribe 분리)
- **SDP-free**: 서버는 SDP를 파싱/조립하지 않음. 코덱/PT/extmap은 서버 정책으로 고정
- **Bundle**: 모든 미디어가 단일 UDP 포트로 멀티플렉싱
- **ICE-Lite**: 서버는 passive controlled agent (STUN binding request 미전송)

```
Browser (WebRTC)
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
3. **SDP 조립은 클라이언트 책임** — 서버 정책 JSON → fake remote SDP → setRemoteDescription → createAnswer
4. **코덱/PT/extmap은 서버 정책으로 고정** — 협상이 아니라 통보

## Lookup Architecture

Three O(1) lookup paths — 2PC 구조에서 (Participant, PcType)으로 식별:

```
[Signaling]  room_id + user_id  →  Participant                    (WS handler)
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

## Media Relay Path

```
UDP recv_from(addr)
  → demux (RFC 5764 first-byte)
  → STUN: latch_by_ufrag → (Participant, PcType) 식별 → Binding Response → DTLS 트리거
  → DTLS: 해당 PC session에 키 설치 (publish/subscribe 독립)
  → SRTP: find_by_addr → PcType::Publish인 경우만 처리
          → decrypt (publish.inbound_srtp)
          → fan-out to room
          → encrypt (target.subscribe.outbound_srtp)
          → send_to (target.subscribe.address)
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

## Signaling Protocol (SDP-free)

Packet format:
```json
{ "op": 11, "pid": 42, "d": { ... } }
{ "op": 11, "pid": 42, "ok": true, "d": { ... } }
{ "op": 11, "pid": 42, "ok": false, "d": { "code": 2001, "msg": "room not found" } }
```

### Client → Server

| op | Name | Description |
|----|------|-------------|
| 1 | HEARTBEAT | Connection keepalive |
| 3 | IDENTIFY | Auth with token |
| 9 | ROOM_LIST | List available rooms |
| 10 | ROOM_CREATE | Create a room |
| 11 | ROOM_JOIN | Join room → returns server_config (ICE, DTLS, codecs, extmap) |
| 12 | ROOM_LEAVE | Leave room |
| 15 | PUBLISH_TRACKS | Register my tracks (SSRC, kind) |
| 20 | MESSAGE | Send data message |

### Server → Client

| op | Name | Description |
|----|------|-------------|
| 0 | HELLO | Connection established, heartbeat interval |
| 100 | ROOM_EVENT | Participant join/leave |
| 101 | TRACKS_UPDATE | Track add/remove (triggers subscribe re-negotiation) |
| 103 | MESSAGE_EVENT | Incoming message |

### ROOM_JOIN Response (server_config)

```json
{
  "room_id": "...",
  "participants": ["userA", "userB"],
  "server_config": {
    "ice": {
      "publish_ufrag": "svr_pub_xxx", "publish_pwd": "...",
      "subscribe_ufrag": "svr_sub_xxx", "subscribe_pwd": "...",
      "ip": "1.2.3.4", "port": 19740
    },
    "dtls": { "fingerprint": "sha-256 AA:BB:...", "setup": "passive" },
    "codecs": [ ... ],
    "extmap": [ ... ]
  },
  "tracks": [ ... ]
}
```

## Project Structure

```
src/
├── main.rs                 Entry point
├── lib.rs                  Server bootstrap (cert gen + WS + UDP)
├── config.rs               Global constants
├── error.rs                Error types (1xxx~9xxx)
├── state.rs                Shared AppState (RoomHub + ServerCert)
├── signaling/
│   ├── opcode.rs           Opcode constants
│   ├── message.rs          Packet types & payloads
│   └── handler.rs          WS lifecycle, opcode dispatch, broadcast
├── transport/
│   ├── demux.rs            Packet classifier (RFC 5764 first-byte)
│   ├── stun.rs             STUN parser/builder (RFC 8489)
│   ├── ice.rs              ICE-Lite (credential gen)
│   ├── demux_conn.rs       Virtual Conn adapter (mpsc ↔ DTLSConn bridge)
│   ├── dtls.rs             DTLS session (cert, fingerprint, handshake, key export)
│   ├── srtp.rs             SRTP context (encrypt/decrypt, key install)
│   └── udp.rs              Single-port UDP listener with 2PC demux dispatch
├── media/
│   ├── router.rs           SSRC routing table (reserved)
│   └── track.rs            Track context (reserved)
└── room/
    ├── room.rs             Room (3-index with PcType) + RoomHub (reverse indices)
    └── participant.rs      PcType + MediaSession + Participant (2PC)
```

## Build & Run

```bash
cargo build
cargo test
RUST_LOG=debug cargo run
```

## Design Targets

- Room capacity: up to 30 participants
- Single UDP port for all media (Bundle)
- ICE-Lite (server never initiates connectivity checks)
- 2PC: publish PC 불변, subscribe PC에서만 re-negotiation
- Conference mode first, PTT support planned
- Versioning: 0.1.x (patch increment per phase)
