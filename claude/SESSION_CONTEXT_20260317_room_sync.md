# SESSION_CONTEXT — 2026-03-17 ROOM_SYNC 트랙 동기화 안전망

> author: kodeholic (powered by Claude)

---

## 세션 목표

TRACKS_UPDATE 시 subscribe 트랙 동기화 안 되는 이슈 해결

---

## 근본 원인 분석

4인 Conference(U912, U515, U311, U077) 테스트에서 **U077만 subscribe 트랙 0개** 현상 발생.

**원인: TRACKS_ACK/RESYNC 무한루프**

1. U077 입장 시 `initial tracks=0` → `_setupSubscribePc` 스킵
2. `sendTracksAck(ssrcs=[])` → 서버 expected set과 mismatch → RESYNC 발송
3. 동시에 TRACKS_UPDATE(U515, U311, U912) 연달아 도착
4. ACK와 RESYNC가 서로 교차하면서 **서버 expected set이 변하는 중에 비교** → 영원히 mismatch
5. **매번 RESYNC가 subscribe PC를 파괴+재생성** → 잘 되던 트랙도 깨뜨림
6. RESYNC 3연발 로그 확인: 동일한 6개 트랙으로 PC 파괴 반복

**텔레메트리 증거:**
- U077: subscribe 항목 0개, subPc 없음 (PTT DIAGNOSTICS에서 빠짐)
- 다른 3명: 정상 (subscribe 3명씩, subPc connected)

---

## 해결 방안: ROOM_SYNC 폴링 안전망

**설계 원칙:**
- TRACKS_UPDATE는 즉시 처리 (빠른 경로 유지)
- sendTracksAck 제거 → RESYNC 무한루프 원인 차단
- TRACKS_RESYNC 수신 시 PC 재생성 안 함 → "동기화 틀어짐" 신호로만 취급
- 2초 디바운스 후 ROOM_SYNC로 서버 ground truth pull → diff → 필요 시 재구성
- 1분 주기 폴링 (마지막 ROOM_SYNC 이후 1분 뒤)

**동작 흐름:**
```
TRACKS_UPDATE → onTracksUpdate (즉시 subscribe re-nego)
             → 2초 디바운스 → ROOM_SYNC 요청 → diff → 필요 시 재구성

TRACKS_RESYNC → PC 재생성 안 함
             → 2초 디바운스 → ROOM_SYNC 요청 → diff → 필요 시 재구성

1분 폴링 → ROOM_SYNC 요청 → diff → 필요 시 재구성
```

---

## 완료 사항 (구현 완료, 테스트 미완)

### 서버 (oxlens-sfu-server) — cargo check 통과 ✅

| 파일 | 변경 |
|------|------|
| `opcode.rs` | `ROOM_SYNC = 50` 추가 |
| `handler.rs` | 디스패치 1줄 + `handle_room_sync` 핸들러 신규 |

`handle_room_sync`: room_join의 tracks 빌드 로직 재활용
- Conference: 다른 참여자들의 실제 트랙 (rtx_ssrc 포함)
- PTT: 가상 SSRC 2개 (audio/video)
- 응답: `{ room_id, mode, participants, subscribe_tracks, floor, total }`

### 클라이언트 (oxlens-home)

| 파일 | 변경 |
|------|------|
| `constants.js` | `ROOM_SYNC: 50` 추가 |
| `signaling.js` | ROOM_SYNC 응답 → `_onRoomSync()` 라우팅 |
| `client.js` | `sendTracksAck()` 호출 3곳 제거 |
| | `_onTracksResync` → PC 재생성 없이 디바운스 ROOM_SYNC만 |
| | `_scheduleSyncDebounce()` — 2초 디바운스 |
| | `_requestRoomSync()` — ROOM_SYNC 요청 |
| | `_onRoomSync(d)` — subscribe_tracks diff 후 필요 시 재구성 |
| | `_startSyncPoll()` — 1분 폴링 (10초 체크, 마지막 sync 이후 1분) |
| | `_stopSyncTimers()` — disconnect/leaveRoom 시 정리 |
| `media-session.js` | `_ackTimer` 잔여물 제거 |
| | `syncSubscribeTracks(serverTracks)` — SSRC set diff → 동일하면 스킵, 다르면 `onTracksResync`로 재구성 |

---

## 미검증 (내일 테스트)

- 4인 Conference 재현 테스트
- 확인 포인트:
  1. `[SDK] requesting ROOM_SYNC` — TRACKS_UPDATE 2초 후 발생
  2. `[MEDIA] syncSubscribeTracks: already in sync` — 정상 시
  3. `syncSubscribeTracks: diff detected` — 누락 복구 시
  4. RESYNC 무한루프 없음 확인

---

## 커밋 메시지 (테스트 후)

서버:
```
feat(signaling): add ROOM_SYNC (op=50) for track sync safety net

- handle_room_sync: participants + subscribe_tracks + floor ground truth
- Reuses room_join track building logic (Conference actual SSRC, PTT virtual SSRC)
```

클라이언트:
```
fix(sync): replace ACK/RESYNC loop with ROOM_SYNC polling safety net

- Remove sendTracksAck calls (cause of RESYNC infinite loop)
- TRACKS_RESYNC now treated as "out of sync" signal only (no PC rebuild)
- 2s debounce after TRACKS_UPDATE/RESYNC → ROOM_SYNC request
- 1min periodic ROOM_SYNC polling as final safety net
- syncSubscribeTracks: SSRC set diff → rebuild only if different
```

---

## 주의사항 (다음 세션 Claude에게)

1. **sendTracksAck()은 dead code** — media-session.js에 메서드는 남아있으나 client.js에서 호출 안 함. 나중에 정리.

2. **onTracksResync()도 여전히 존재** — media-session.js에서 syncSubscribeTracks가 내부적으로 호출함. 제거하지 말 것.

3. **서버 TRACKS_ACK 핸들러도 남아있음** — 클라이언트가 안 보내므로 발동 안 됨. 서버 코드는 안 건드림.

4. **admin 대시보드** — `demo/admin/` 6파일 모듈 구조. tracks_ack_mismatch, tracks_resync_sent 메트릭은 이제 항상 0일 것.

---

*최종 갱신: 2026-03-17*
*author: kodeholic (powered by Claude)*