---
name: light-livechat
description: |
  Rust + Tokio + Axum 기반 SFU (Selective Forwarding Unit) 서버 프로젝트.
  Conference 모드 우선, PTT 추후 지원. mini-livechat의 후속 프로젝트.
  "light-livechat", "SFU", "light sfu", "DTLS", "SRTP 릴레이", "Room", "RoomHub",
  "Participant", "DemuxConn", "ICE-Lite", "SDP answer" 등의 키워드가 나오면 이 스킬을 참조할 것.
---

# Light LiveChat — 프로젝트 지침서

## 1. 프로젝트 개요

- **목적**: 경량 Conference SFU 서버 (PTT 추후 확장)
- **언어/프레임워크**: Rust + Tokio + Axum
- **로컬 경로**: `D:\X.WORK\GitHub\repository\light-livechat\`
- **참조 프로젝트**: `D:\X.WORK\GitHub\repository\mini-livechat\` (검증된 패턴 이식 원본)
- **설계 규모**: 방당 최대 20명, 단일 인스턴스

---

## 2. 작업 원칙

### 2.1 mini-livechat 참조 패턴

mini-livechat에서 검증된 코드를 이식할 때:
1. **API 추측 금지** — 반드시 mini 소스를 직접 읽고 확인
2. **크레이트 버전 통일** — 0.17.x 계열 유지 (dtls, webrtc-srtp, webrtc-util)
3. **구조는 단순화** — mini의 이원 구조(MediaPeerHub + ChannelHub)를 RoomHub 일원 구조로 통합
4. **re-nego는 최대한 늦게, 최대한 단순하게** — mini가 여기서 실패함

### 2.2 Phase 기반 점진적 구현

- 한 Phase에서 하나의 관심사만 다룬다
- Phase 완료 시 반드시: `cargo build` 클린 → 문서 갱신 → 버전 올림 → 커밋
- Phase 간 의존: 이전 Phase 빌드 성공이 다음 Phase 진입 조건

### 2.3 설계 토론 → 코딩 순서

1. **구조 논의**: 뭘 만들지, 왜 이렇게 만드는지 합의
2. **기존 코드 확인**: mini-livechat 소스 + light-livechat 현재 상태 읽기
3. **코딩**: 합의된 구조대로 작성
4. **빌드 확인**: 부장이 `cargo build` 실행하여 결과 공유
5. **에러 수정**: 빌드 에러 메시지 기반으로 정확히 수정 (추측 금지)
6. **문서 갱신**: CHANGELOG, TODO, README, 버전

### 2.4 빌드 에러 대응

- 에러 메시지를 **전문 그대로** 받아서 수정 (컴파일러가 맞다)
- API 시그니처 추측 → 빌드 에러 → 핑퐁 반복 **금지**
- 모르면 mini-livechat 소스에서 검증된 패턴 확인 후 이식
- 워닝도 즉시 정리 (unused import, unused variable)

---

## 3. 아키텍처

### 3.1 전체 구조

```
Browser (WebRTC)
    ├── WebSocket ──→ [Axum WS] ── Signaling (SDP Offer/Answer, ICE candidate)
    └── UDP ────────→ [Single Port] ── Media
                          ├── ICE-Lite  (STUN binding response)
                          ├── DTLS 1.2  (handshake → SRTP key derivation)
                          └── SRTP      (decrypt → route → encrypt)
```

### 3.2 O(1) 3중 인덱스 조회

```
[Signaling]  room_id + user_id  →  Participant    O(1)
[STUN]       ufrag              →  room_id → Participant    O(1) × 2
[SRTP]       SocketAddr         →  room_id → Participant    O(1) × 2
```

```
RoomHub
  ├── rooms:       DashMap<room_id, Room>
  ├── ufrag_index: DashMap<ufrag, room_id>       ← STUN 역인덱스
  └── addr_index:  DashMap<addr, room_id>        ← SRTP 역인덱스

Room
  ├── participants: DashMap<user_id, Participant>  ← primary
  ├── by_ufrag:     DashMap<ufrag, Participant>    ← STUN
  └── by_addr:      DashMap<addr, Participant>     ← SRTP
```

### 3.3 미디어 파이프라인

```
UDP recv_from(addr)
  → demux (RFC 5764 first-byte)
  → STUN:  parse → latch_by_ufrag → Binding Response → USE-CANDIDATE → DTLS 트리거
  → DTLS:  DemuxConn inject → DTLSConn handshake → export_srtp_keys → install
  → SRTP:  find_by_addr O(1) → decrypt_rtp → fan-out to room → encrypt_rtp → send_to
```

---

## 4. 소스 구조

```
src/
├── main.rs                 Entry point
├── lib.rs                  Server bootstrap (cert gen + WS + UDP)
├── config.rs               Global constants
├── error.rs                Error types (1xxx~9xxx)
├── state.rs                AppState (RoomHub + Router + ServerCert)
├── signaling/
│   ├── opcode.rs           Opcode constants
│   ├── message.rs          Packet types & payloads
│   └── handler.rs          WS lifecycle, dispatch, broadcast
├── transport/
│   ├── demux.rs            RFC 5764 first-byte classifier
│   ├── stun.rs             STUN parser/builder (RFC 8489)
│   ├── ice.rs              ICE-Lite (credential gen, binding handler)
│   ├── demux_conn.rs       Conn trait adapter (mpsc ↔ DTLSConn bridge)
│   ├── dtls.rs             DTLS (cert, fingerprint, handshake, key export)
│   ├── srtp.rs             SRTP context (encrypt/decrypt)
│   ├── sdp.rs              SDP Offer parsing + Answer generation
│   └── udp.rs              Single-port UDP loop + RoomHub integration
├── media/
│   ├── router.rs           SSRC routing table
│   └── track.rs            Track context
└── room/
    ├── room.rs             Room (3-index) + RoomHub (reverse indices)
    └── participant.rs      Participant (ICE + SRTP + tracks)
```

---

## 5. 크레이트 버전 (고정)

```toml
dtls        = "0.17.1"   # webrtc-dtls 리네이밍, Tokio 기반 최종 안정판
webrtc-srtp = "0.17.1"
webrtc-util = "0.17.1"
bytes       = "1"
async-trait = "0.1"
```

**절대 0.18+ 사용 금지** — Sans-IO 전환으로 API 완전히 다름. mini-livechat 검증 버전 유지.

---

## 6. 시그널링 프로토콜

### 패킷 형식
```json
{ "op": 11, "pid": 42, "d": { ... } }                    // 요청/이벤트
{ "op": 11, "pid": 42, "ok": true, "d": { ... } }        // 성공 응답
{ "op": 11, "pid": 42, "ok": false, "d": { "code": N } } // 에러 응답
```

### Client → Server
| op | Name | 설명 |
|----|------|------|
| 1 | HEARTBEAT | keepalive |
| 3 | IDENTIFY | 인증 (token → user_id) |
| 10 | ROOM_CREATE | 방 생성 |
| 11 | ROOM_JOIN | 입장 (ICE params + DTLS fingerprint 응답) |
| 12 | ROOM_LEAVE | 퇴장 |
| 15 | SDP_OFFER | 재협상 (Phase 3+) |
| 16 | ICE_CANDIDATE | Trickle ICE (ICE-Lite에서 무시) |
| 20 | MESSAGE | 데이터 메시지 |

### Server → Client
| op | Name | 설명 |
|----|------|------|
| 0 | HELLO | heartbeat_interval |
| 100 | ROOM_EVENT | participant_joined / participant_left |
| 101 | TRACK_EVENT | track_added / track_removed |
| 103 | MESSAGE_EVENT | 메시지 릴레이 |

---

## 7. 구현 로드맵

| Phase | 내용 | 버전 | 상태 |
|-------|------|------|------|
| 0 | STUN / ICE-Lite | 0.1.1 | ✅ |
| 1 | DTLS + SRTP 모듈 | 0.1.2 | ✅ |
| 1.5 | Room 3-index + 시그널링 핸들러 | 0.1.2 | ✅ |
| 2 | UDP ↔ RoomHub 통합 (전체 파이프라인) | 0.1.3 | ✅ |
| 3 | SDP Negotiation (브라우저 연동) | 0.1.4 | ✅ |
| 4 | RTP 라우팅 정제 | 0.1.5 | |
| 5 | Hardening (인증, 좀비, 타임아웃) | 0.1.6 | |
| 6 | PTT 지원 | 0.1.7 | |
| 7 | Simulcast / SVC (optional) | 0.2.x | |

---

## 8. 코딩 규칙

- 파일 상단 `// author: kodeholic (powered by Claude)` 명시
- 매직 넘버 금지 → `config.rs` 상수 사용
- `unwrap()` 남용 금지 → `Result` 전파 또는 로그 후 `continue`
- 워닝 0 유지 (unused import, unused variable 즉시 정리)
- 새 Phase 완료 시 CHANGELOG.md + TODO.md + README.md 갱신
- 코딩은 **"코딩해줘" 명시적 요청 시에만** 작성
- 코딩 전 요구사항/구조 합의 필수

---

## 9. 버전 관리

- `0.1.x`: Phase별 patch increment
- Phase 완료 = 버전 올림 + 커밋
- 커밋 메시지: `"v0.1.N: Phase X description"` + 변경사항 bullet

---

## 10. 주의사항

### 절대 하지 말 것
- re-nego 성급하게 구현 (mini 실패 원인)
- 크레이트 API 추측으로 코드 작성 (빌드 에러 핑퐁)
- 0.18+ webrtc-rs 크레이트 사용
- Phase 건너뛰기 (이전 Phase 빌드 성공 없이 다음 진행)

### 항상 할 것
- 새 모듈 작성 전 mini-livechat 해당 소스 확인
- 빌드 에러는 에러 메시지 전문 기반으로 수정
- 설계 결정 시 대안과 trade-off 명시
- 20명 규모 기준으로 성능 판단 (과도한 최적화 경계)
