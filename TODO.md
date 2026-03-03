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

## Phase 2: UDP ↔ RoomHub Integration ← NEXT
- [ ] STUN handler uses RoomHub.latch_by_ufrag() instead of per-transport credentials
- [ ] ICE USE-CANDIDATE → trigger DTLS handshake for participant
- [ ] DTLS handshake complete → install SRTP keys on Participant
- [ ] SRTP handler: find_by_addr() → decrypt → relay to room → encrypt → send
- [ ] RTCP passthrough (decrypt for logging, no relay)
- [ ] AppState shared between WS handlers and UDP transport

## Phase 3: RTP Routing (Media Pipeline)
- [ ] RTP header parsing (SSRC, PT, sequence, timestamp)
- [ ] Fan-out: sender → all subscribers in room (exclude sender)
- [ ] SSRC rewrite (optional, for simulcast/SVC prep)
- [ ] Hot-path optimization (minimize allocations)

## Phase 4: Full SDP Negotiation
- [ ] SDP Offer parsing (webrtc-sdp or hand-rolled)
- [ ] SDP Answer generation (ICE-Lite, DTLS fingerprint, codec mirror)
- [ ] Track registration from SDP (SSRC → TrackKind mapping)
- [ ] Renegotiation (SDP_OFFER opcode → re-answer)
- [ ] TRACK_EVENT notification to room participants

## Phase 5: Hardening
- [ ] IDENTIFY token verification (JWT or shared secret)
- [ ] Zombie session reaper (last_seen timeout)
- [ ] ACK miss counter → connection termination
- [ ] Heartbeat timeout → disconnect
- [ ] DTLS handshake timeout
- [ ] Graceful shutdown (drain connections)
- [ ] Structured logging & metrics

## Phase 6: PTT Support
- [ ] Room mode field (Conference / PTT)
- [ ] Floor control state machine (Idle → Taken → Idle)
- [ ] FLOOR_REQUEST / FLOOR_RELEASE opcodes
- [ ] Relay gate: only floor holder's media is forwarded in PTT mode
- [ ] Floor indicator broadcast

## Phase 7: Simulcast / SVC (Optional)
- [ ] Simulcast layer detection (RID / SSRC grouping)
- [ ] Layer selection per subscriber
- [ ] Adaptive quality switching

## Backlog
- [ ] TURN relay support (for restrictive NATs)
- [ ] Recording (RTP → file)
- [ ] Data channel support
- [ ] Horizontal scaling (multi-node)
