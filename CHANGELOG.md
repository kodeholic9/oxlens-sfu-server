# Changelog

All notable changes to this project will be documented in this file.

Format follows [Keep a Changelog](https://keepachangelog.com/).

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
