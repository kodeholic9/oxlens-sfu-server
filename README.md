# light-livechat

High-performance SFU (Selective Forwarding Unit) server built with Rust.

## Architecture

- **Bundle**: All media streams multiplexed over a single UDP transport per participant
- **ICE-Lite**: Server acts as passive controlled agent (no STUN binding requests)
- **WebSocket Signaling**: Custom opcode-based protocol carrying SDP for media negotiation

```
Browser (WebRTC)
    ├── WebSocket ──→ [Axum WS] ── Signaling (SDP Offer/Answer, ICE candidate)
    └── UDP ────────→ [Single Port] ── Media
                          ├── ICE-Lite  (STUN binding response)
                          ├── DTLS 1.2  (handshake → SRTP key derivation)
                          └── SRTP      (decrypt → route → encrypt)
```

## Lookup Architecture

Three O(1) lookup paths into the same `Arc<Participant>`:

```
[Signaling]  room_id + user_id  →  Participant    (WS handler)
[STUN]       ufrag              →  room_id  →  Participant    (ICE cold path)
[SRTP]       SocketAddr         →  room_id  →  Participant    (media hot path)
```

```
RoomHub
  ├── rooms:       DashMap<room_id, Room>
  ├── ufrag_index: DashMap<ufrag, room_id>       ← STUN reverse index
  └── addr_index:  DashMap<addr, room_id>        ← SRTP reverse index

Room
  ├── participants: DashMap<user_id, Participant>  ← primary
  ├── by_ufrag:     DashMap<ufrag, Participant>    ← STUN cold path
  └── by_addr:      DashMap<addr, Participant>     ← SRTP hot path
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

## Signaling Protocol

Packet format — all messages carry a sequential `pid` for request-response pairing:

```json
{ "op": 11, "pid": 42, "d": { ... } }
{ "op": 11, "pid": 42, "ok": true, "d": { ... } }
{ "op": 11, "pid": 42, "ok": false, "d": { "code": 2001, "msg": "room not found" } }
```

Rules:
- `pid`: always present, sequential u64, per-side counter starting from 1
- `ok`: present only in responses (true=success, false=error)
- Both sides use go-back-n — multiple requests can be in-flight simultaneously

### Client → Server (Request)

| op | Name | Description |
|----|------|-------------|
| 1 | HEARTBEAT | Connection keepalive |
| 3 | IDENTIFY | Auth with token |
| 10 | ROOM_CREATE | Create a room |
| 11 | ROOM_JOIN | Join room with SDP Offer (returns SDP Answer) |
| 12 | ROOM_LEAVE | Leave room |
| 15 | SDP_OFFER | Renegotiation |
| 16 | ICE_CANDIDATE | Trickle ICE (acknowledged, ignored in ICE-Lite) |
| 20 | MESSAGE | Send data message |

### Server → Client (Event)

| op | Name | Description |
|----|------|-------------|
| 0 | HELLO | Connection established, heartbeat interval |
| 100 | ROOM_EVENT | Participant join/leave |
| 101 | TRACK_EVENT | Track added/removed |
| 102 | ICE_CANDIDATE | Server ICE candidate |
| 103 | MESSAGE_EVENT | Incoming message from others |

## Project Structure

```
src/
├── main.rs                 Entry point
├── lib.rs                  Server bootstrap (cert gen + WS + UDP)
├── config.rs               Global constants
├── error.rs                Error types (1xxx~9xxx)
├── state.rs                Shared AppState (RoomHub + Router + ServerCert)
├── signaling/
│   ├── opcode.rs           Opcode constants
│   ├── message.rs          Packet types & payloads
│   └── handler.rs          WS lifecycle, opcode dispatch, broadcast
├── transport/
│   ├── demux.rs            Packet classifier (RFC 5764 first-byte)
│   ├── stun.rs             STUN parser/builder (RFC 8489)
│   ├── ice.rs              ICE-Lite handler (credential gen, binding response)
│   ├── demux_conn.rs       Virtual Conn adapter (mpsc ↔ DTLSConn bridge)
│   ├── dtls.rs             DTLS session (cert, fingerprint, handshake, key export)
│   ├── srtp.rs             SRTP context (encrypt/decrypt, key install)
│   ├── sdp.rs              SDP Offer parsing + Answer generation
│   └── udp.rs              Single-port UDP listener with demux dispatch
├── media/
│   ├── router.rs           SSRC routing table
│   └── track.rs            Track context
└── room/
    ├── room.rs             Room (3-index) + RoomHub (reverse indices)
    └── participant.rs      Participant (ICE + SRTP + tracks)
```

## Build & Run

```bash
cargo build
cargo test
RUST_LOG=debug cargo run
```

상세 실행/배포/모니터링은 [OPERATIONS.md](OPERATIONS.md) 참조.

## Design Targets

- Room capacity: up to 20 participants
- Single UDP port for all media (Bundle)
- ICE-Lite (server never initiates connectivity checks)
- Conference mode first, PTT support planned
- Versioning: 0.1.x (patch increment per phase)
