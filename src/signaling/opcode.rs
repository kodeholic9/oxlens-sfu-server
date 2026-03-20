// author: kodeholic (powered by Claude)
//! Opcode definitions for signaling protocol (2PC / SDP-free)
//!
//! Request/Response: Client sends request, Server responds with same op + ok field.
//! Event:           Server sends event, Client responds with same op + ok field.
//! All messages carry a sequential `pid` for request-response pairing.

// --- Client → Server (Request) ---
pub const HEARTBEAT: u16 = 1;
pub const IDENTIFY: u16 = 3;
pub const ROOM_LIST: u16 = 9;
pub const ROOM_CREATE: u16 = 10;
pub const ROOM_JOIN: u16 = 11;
pub const ROOM_LEAVE: u16 = 12;
pub const PUBLISH_TRACKS: u16 = 15;   // 클라이언트가 자기 트랙 SSRC 등록
pub const TRACKS_ACK: u16 = 16;       // 클라이언트가 인식한 subscribe SSRC 확인 응답
pub const MUTE_UPDATE: u16 = 17;      // 트랙 mute/unmute 상태 변경
pub const CAMERA_READY: u16 = 18;     // 카메라 웜업 완료 (첫 프레임 → PLI 트리거)
pub const MESSAGE: u16 = 20;
pub const TELEMETRY: u16 = 30;          // 클라이언트 telemetry 보고
pub const ROOM_SYNC: u16 = 50;           // 참여자+트랙+floor 전체 동기화 (폴링)
pub const SUBSCRIBE_LAYER: u16 = 51;     // Simulcast 레이어 선택 (Phase 3에서 구현)

// --- Floor Control (MCPTT/MBCP) ---
pub const FLOOR_REQUEST: u16 = 40;      // 발화권 요청 (PTT 누름)
pub const FLOOR_RELEASE: u16 = 41;      // 발화권 자진 해제 (PTT 뗌)
pub const FLOOR_PING: u16 = 42;         // 발화자 생존 확인 (T_FLOOR_PING 주기)

// --- Server → Client (Event) ---
pub const HELLO: u16 = 0;
pub const ROOM_EVENT: u16 = 100;
pub const TRACKS_UPDATE: u16 = 101;   // 트랙 추가/제거 통보
pub const TRACK_STATE: u16 = 102;     // 트랙 mute/unmute 상태 브로드캐스트
pub const MESSAGE_EVENT: u16 = 103;
pub const VIDEO_SUSPENDED: u16 = 104;   // 비디오 중단 (카메라 off) — UI avatar 전환
pub const TRACKS_RESYNC: u16 = 106;    // 트랙 목록 전체 재동기화 (TRACKS_ACK 불일치 시)
pub const VIDEO_RESUMED: u16 = 105;     // 비디오 재개 (카메라 on) — UI 복원
pub const ADMIN_TELEMETRY: u16 = 110;   // 서버 → 어드민 telemetry 중계

// --- Floor Control Events (MCPTT/MBCP) ---
pub const FLOOR_TAKEN: u16 = 141;       // 누군가 발화권 획득 (브로드캐스트)
pub const FLOOR_IDLE: u16 = 142;        // 발화권 해제, 채널 비어있음 (브로드캐스트)
pub const FLOOR_REVOKE: u16 = 143;      // 서버가 강제로 발화권 회수
