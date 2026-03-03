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

## Phase 3: SDP Negotiation ✔
- [x] SDP Offer parsing (extract codecs, SSRC, mid, direction, extmap, rtcp-rsize)
- [x] SDP Answer generation (ICE-Lite params, passive DTLS fingerprint, codec mirror, host candidate)
- [x] ROOM_JOIN flow: sdp_offer required → parse → answer → response
- [x] ROOM_LIST opcode + 서버 기본 방 생성
- [x] 클라이언트 SDK + UI (insight-lens) light 프로토콜 전환
- [x] 브라우저 통합 테스트 (WS→SDP→ICE→DTLS→SRTP relay 확인, 3명 그리드)
- [ ] Track registration (SSRC → TrackKind mapping) — deferred to Phase 4

## Phase 3.5: Debug Logging + Video Rendering Fix ✅
- [x] Server debug tags: `[DBG:SDP]`, `[DBG:STUN]`, `[DBG:DTLS]`, `[DBG:RTP]`, `[DBG:RELAY]`, `[DBG:RTCP]`
- [x] Client debug tags: `[DBG:SDP]`, `[DBG:ICE]`, `[DBG:TRACK]`, `[DBG:RTP]`
- [x] RTP header parser (logging only): SSRC, PT, seq, timestamp, marker, payload_len
- [x] Log throttling: AtomicU64 counter, first 50 detailed, 1000-interval summary
- [x] Client inbound-rtp stats monitor (3s getStats polling)
- [x] Video rendering fix: remoteVideoStream deferred attach (ontrack ↔ tile timing)

## Phase 4: RTP Routing Refinement
- [ ] **RTCP 릴레이** — 근본 원인: Chrome congestion control이 피드백 없으면 비트레이트 안 올림 (화질 저하)
- [ ] **PLI 생성/전송** — 새 참가자 입장 시 키프레임 요청 (첫 프레임 지연 해결)
- [ ] **SSRC → user_id 매핑** — 3명+ 지원 기반
- [ ] SSRC rewrite (optional, for simulcast/SVC prep)
- [ ] Hot-path optimization (minimize allocations, Bytes zero-copy)
- [ ] Bandwidth estimation (basic)

## Phase 5: Hardening
- [ ] IDENTIFY token verification (JWT or shared secret)
- [ ] Zombie session reaper (last_seen timeout)
- [ ] ACK miss counter → connection termination
- [ ] Heartbeat timeout → disconnect
- [ ] DTLS handshake timeout cleanup
- [ ] Graceful shutdown (drain connections)
- [ ] Structured logging & metrics

## Phase 6: PTT Support
- [ ] Room mode field (Conference / PTT)
- [ ] Floor control state machine (Idle → Taken → Idle)
- [ ] FLOOR_REQUEST / FLOOR_RELEASE opcodes
- [ ] Relay gate: only floor holder's media forwarded in PTT mode
- [ ] Floor indicator broadcast

## Phase 7: Simulcast / SVC (Optional)
- [ ] Simulcast layer detection (RID / SSRC grouping)
- [ ] Layer selection per subscriber
- [ ] Adaptive quality switching

## Backlog
- [ ] Renegotiation (track add/remove without re-join)
- [ ] TURN relay support (for restrictive NATs)
- [ ] Recording (RTP → file)
- [ ] Data channel support
- [ ] Horizontal scaling (multi-node)
