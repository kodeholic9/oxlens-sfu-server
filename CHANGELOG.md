# Changelog

All notable changes to this project will be documented in this file.

Format follows [Keep a Changelog](https://keepachangelog.com/).

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
