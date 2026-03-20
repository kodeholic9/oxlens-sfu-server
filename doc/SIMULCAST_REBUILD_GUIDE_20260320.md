# OxLens Simulcast — 처음부터 다시 만들기 가이드

> author: kodeholic (powered by Claude)
> 작성일: 2026-03-20
> 목적: Phase 0~3 재구현 시 이전 실수를 반복하지 않기 위한 전체 설계/구현/검증 가이드

---

## 핵심 원칙 (이 문서 전체를 관통하는 룰)

### 1. WebRTC 관점에서 생각한다
SFU는 WebRTC의 규약을 지키는 것이 전부다. Chrome이 기대하는 것:
- **SDP에 선언된 SSRC로 패킷이 와야 한다** — 다른 SSRC는 무시/드롭
- **키프레임 없이 P-frame만 오면 디코딩 불가** — 검은 화면
- **seq/ts가 불연속이면 jitter buffer가 폭등** — 지연 심화
- **PLI를 보내면 키프레임이 와야 한다** — 안 오면 영구 stuck

### 2. 추측 코딩 금지, 로그 먼저
문제 발생 시 **절대** 추측으로 코드를 고치지 않는다.
- 서버 디버그 로그 요청
- 클라이언트 브라우저 콘솔 로그 요청
- 텔레메트리 스냅샷 분석
- 로그에서 원인이 확정된 후에만 코딩

### 3. 설계 → 체크리스트 → 코딩 → 빌드 → 테스트 (한 호흡에)
설계에서 "5곳 수정"이라 했으면 5곳을 **한 턴에** 전부 코딩한다.
"다음 턴에서" 미루면 반드시 빠진다. 파일 간 이동하면서 놓친다.

### 4. 기존 경로 절대 깨뜨리지 않기
`simulcast_enabled == false`이면 기존 코드 100% 그대로 동작해야 한다.
PTT 모드에서도 마찬가지.

---

## 상용 SFU Simulcast 구독 모델 참조

### mediasoup
- `consumer.setPreferredLayers({spatialLayer})` + `pause()/resume()`
- Consumer별 **가상 SSRC 랜덤 할당** (subscriber는 이 SSRC만 앎)
- 서버가 h/l 전환 시 SSRC/seq/ts rewrite → subscriber 투명

### LiveKit
- `UpdateTrackSettings {disabled, quality, width, height}`
- AdaptiveStream: UI 크기/visibility 자동 감지 → 레이어 선택
- Dynacast: 미소비 레이어 publish 중단

### Janus
- `configure {substream, temporal, send}`
- subscriber가 직접 API 호출

### 공통 패턴
- 클라이언트 → 서버 시그널링으로 레이어 선택
- **서버가 SSRC/seq/ts rewrite** — subscriber에게 단일 video track만 노출
- pause 시 RTP 중단 → resume 시 PLI → 키프레임부터 재개

---

## Phase 0 — 구조 준비 (행동 변경 Zero)

### 목표
simulcast 관련 필드/구조체만 추가. 기존 동작에 어떤 변화도 없음.

### 서버 변경

**config.rs** — 상수 추가
```rust
pub const SIMULCAST_HIGH_MAX_BITRATE: u32 = 1_650_000;
pub const SIMULCAST_LOW_MAX_BITRATE: u32 = 250_000;
pub const SIMULCAST_LOW_SCALE_DOWN: u32 = 4;
```

**participant.rs** — Track 구조체에 rid/simulcast_group 추가
```rust
pub struct Track {
    // 기존 필드 유지
    pub rid: Option<String>,           // "h", "l", None
    pub simulcast_group: Option<u32>,
}
```

**participant.rs** — `add_track_ext()` 메서드 추가 (기존 `add_track()` 보존)

**room.rs** — Room에 `simulcast_enabled: bool` 필드

**message.rs** — `RoomCreateRequest`에 `simulcast: Option<bool>`

**handler.rs** — `handle_room_create`에서 simulcast 파라미터 전달

### 검증
- cargo build 성공
- 기존 2인 양방향 테스트 통과
- PTT 테스트 통과
- **Contract 체크 12항목 ALL PASS**

---

## Phase 1 — Publish PC client-offer 전환 + SSRC 등록

### 배경
Chrome answerer에서는 simulcast sendEncodings의 rid가 SDP에 안 나옴.
Chrome offerer로 전환해야 simulcast SDP가 제대로 생성됨.

### 클라이언트 변경 (media-session.js)

**Publish PC 생성**:
- audio: `addTransceiver(audioTrack, { direction: 'sendonly' })` ← **sendrecv 금지**
- video (simulcast ON): `addTransceiver(videoTrack, { direction: 'sendonly', sendEncodings: [...] })`
- video (simulcast OFF): `addTransceiver(videoTrack, { direction: 'sendonly' })` ← **addTrack 금지**

**SDP 협상 순서 전환**:
```
기존: server offer → setRemoteDescription → createAnswer → setLocalDescription
전환: createOffer → setLocalDescription → buildPublishRemoteAnswer → setRemoteDescription("answer")
```

**SSRC 추출**:
- Chrome offerer는 SDP에도 sender.getParameters()에도 simulcast SSRC 미노출
- `getStats()` outbound-rtp에서 rid별 SSRC 추출 (mediasoup 방식)
- 레이어 부족 시 500ms × 3회 재시도
- connected 시점에 재시도 (pendingPublishTracks 플래그)

**PUBLISH_TRACKS에 twcc_extmap_id 포함** — Chrome offerer가 할당한 ID를 서버에 전달

### 서버 변경 (handler.rs)

**handle_publish_tracks**:
- `twcc_extmap_id` 저장 (`participant.twcc_extmap_id`)
- `rid` 필드 있으면 `add_track_ext()`로 등록

**TRACKS_UPDATE 브로드캐스트**:
- **rid="l" 제외** — subscriber에게 video m-line 1개만 보이도록

**ROOM_JOIN existing_tracks**:
- 동일하게 rid="l" 제외

**server_extmap_policy**:
- simulcast ON → rtp-stream-id(id=10) + repaired-rtp-stream-id(id=11) 추가

### sdp-builder.js

**buildPublishRemoteAnswer()**:
- Chrome offer의 extmap URI→ID 매핑을 파싱해서 answer에 그대로 사용
- simulcast rid/simulcast 라인 포함

### 검증
- simulcast OFF 2인 양방향: 기존과 동일 (regression 없음)
- simulcast ON 2인 양방향: high 레이어만 전달, **12/12 Contract PASS**
- PTT regression: 정상
- `SimulcastEncoderAdapter (libvpx, libvpx)` 확인 (encoder가 2레이어 생성)

---

## Phase 3 — Subscriber별 레이어 선택 + 가상 SSRC + Rewrite

### ⚠️ 이번 Phase가 핵심이자 가장 위험한 단계

### 핵심 설계: 가상 Video SSRC

**왜 가상 SSRC가 필요한가?**

Subscriber는 video m-line 1개만 가짐. SDP에 선언된 단일 SSRC로 패킷이 와야 함.
서버가 h↔l 전환 시 다른 real SSRC의 패킷을 보내면 Chrome이 무시.
→ publisher별 **고정 가상 video SSRC** 할당 → 모든 레이어의 패킷을 이 SSRC로 rewrite

**데이터 흐름**:
```
일반/PTT:  PUBLISH_TRACKS → real SSRC → TRACKS_UPDATE(real) → 기존 경로
Simulcast: PUBLISH_TRACKS → ensure_simulcast_video_ssrc() 할당
            → TRACKS_UPDATE(virtual) 브로드캐스트
            → Subscriber SDP에 virtual SSRC로 빌드
            → ingress: h든 l이든 SimulcastRewriter가 virtual로 rewrite → Chrome 매칭 ✓
```

### SSRC를 참조하는 모든 경로 (이것이 체크리스트)

**가상 SSRC가 도입되면 아래 전체를 한번에 수정해야 한다:**

| # | 경로 | 설명 | 방향 |
|---|------|------|------|
| 1 | `PUBLISH_TRACKS` → `TRACKS_UPDATE` | broadcast 시 h video SSRC → 가상 SSRC 교체, rid 제거 | server→client |
| 2 | `ROOM_JOIN` → `existing_tracks` | 이미 방에 있는 publisher의 h video → 가상 SSRC | server→client |
| 3 | `ROOM_SYNC` → `subscribe_tracks` | 동기화 응답의 h video → 가상 SSRC | server→client |
| 4 | `ROOM_LEAVE` / `cleanup` → `TRACKS_UPDATE(remove)` | 퇴장 broadcast의 h video → 가상 SSRC | server→client |
| 5 | `SUBSCRIBE_LAYER` | SubscribeLayer 생성 시 output_ssrc = 가상 SSRC | server 내부 |
| 6 | `ingress.rs` fan-out | SimulcastRewriter가 모든 패킷을 가상 SSRC로 rewrite | server hot path |
| 7 | `ingress.rs` PLI relay | subscriber Chrome PLI(가상 SSRC) → real SSRC 역매핑 | client→server |
| 8 | `ingress.rs` NACK | subscriber NACK(가상 SSRC, 가상 seq) → real SSRC/seq 역매핑 | client→server |
| 9 | `TRACKS_ACK` expected set | 가상 SSRC 기준으로 비교해야 할 수 있음 | 확인 필요 |
| 10 | SR relay | send_stats에서 가상 SSRC 기준 조회 | 확인 필요 |

### 서버 구현 상세

**participant.rs**:
```rust
// publisher별 고정 가상 video SSRC
pub simulcast_video_ssrc: AtomicU32,  // 0 = 미할당

// CAS 기반 lazy 할당
pub fn ensure_simulcast_video_ssrc(&self) -> u32 {
    let existing = self.simulcast_video_ssrc.load(Ordering::Relaxed);
    if existing != 0 { return existing; }
    let new_ssrc = self.virtual_ssrc_counter.fetch_add(1, Ordering::Relaxed);
    match self.simulcast_video_ssrc.compare_exchange(0, new_ssrc, ...) {
        Ok(_) => new_ssrc,
        Err(winner) => winner,
    }
}
```

**SimulcastRewriter** (participant.rs):
```
- virtual_ssrc: u32           — 고정 출력 SSRC
- last_out_seq / last_out_ts  — 레이어 전환 시 연속 보장
- pending_keyframe: bool      — 전환 후 키프레임 대기
- seq_offset / ts_offset      — 입력→출력 변환 오프셋
- initialized: bool (pub)     — 첫 키프레임 수신 여부
```

**rewrite() 시맨틱 (매우 중요)**:
```
initialized=false + P-frame → false (드롭) ← 키프레임부터 시작해야 Chrome 디코딩 가능
initialized=false + 키프레임 → offset=0, initialized=true, SSRC rewrite, true
pending_keyframe + P-frame → false (드롭)
pending_keyframe + 키프레임 → offset 재계산, SSRC rewrite, true
normal → SSRC/seq/ts rewrite, true
```

**room.rs**:
```rust
// 가상 SSRC로 publisher 찾기 (PLI/NACK 역매핑용)
pub fn find_publisher_by_vssrc(&self, vssrc: u32) -> Option<Arc<Participant>>
```

### PLI 교착 문제 (3단계 deadlock)

이전 세션에서 가장 큰 문제였음. **반드시 이해하고 대책을 구현해야 한다.**

```
1. 첫 RTP 도착 → subscribe_state 비어있음 → SubscribeLayer 즉석 생성 (initialized=false)
2. 첫 패킷이 P-frame → SimulcastRewriter가 드롭 → Chrome에게 0바이트 도착
3. Chrome: "패킷 없으니 PLI 안 보냄" (경로 A 실패)
4. SUBSCRIBE_LAYER(h) 도착 → old_layer==new_layer(h==h) → PLI skip (경로 B 실패)
5. 영구 stuck — 키프레임 영영 안 옴
```

**대책 (2중)**:
1. `ingress.rs`: SubscribeLayer 즉석 생성 시 **즉시 PLI burst 발사** [0,200,500,1500]ms
2. `handler.rs`: SUBSCRIBE_LAYER에서 `old==new`여도 `rewriter.initialized==false`면 **PLI 발사**

### 클라이언트 구현

**client.js**:
- `_simulcastEnabled = false` (constructor)
- `_onJoinOk`: `this._simulcastEnabled = !!(simulcast && simulcast.enabled)`
- `_resetMute()`: `this._simulcastEnabled = false`

**signaling.js**:
- `subscribeLayer(targets)` → `OP.SUBSCRIBE_LAYER` 전송

**app.js**:
- `redistributeTiles()`: simulcast ON이면 main 타일 → h, thumbs → l 전송
- `_setupThumbObserver()`: visibility 변경 시 l/pause 전송
- long-press 팝업: 수동 h/l/pause 선택 (디버그용)
  - `_manualLayerOverride` Set으로 수동 설정 보호 (redistributeTiles가 덮어쓰기 방지)

### 빌드 전 검증 체크리스트

코딩 완료 후, 빌드 전에 아래를 확인:

- [ ] `cargo build --release` 성공 (컴파일 에러 없음)
- [ ] `room.simulcast_enabled`를 클로저 내부에서 사용할 때 `let sim_enabled = room.simulcast_enabled;`로 Copy 타입 캡처
- [ ] `Arc<Room>`을 `move` 클로저에 넣지 않음 (소유권 이동 에러)
- [ ] `SimulcastRewriter.initialized` 필드가 `pub`인지 확인 (handler.rs에서 참조)
- [ ] `ingress.rs`에서 `SubscribeLayer` import 있는지 확인

### 테스트 순서 (반드시 이 순서대로)

1. **simulcast OFF 2인 양방향** — 기존 regression 없는지 확인. loss 0%, 12/12 Contract PASS
2. **PTT regression** — simulcast OFF 방에서 발화권 정상
3. **simulcast ON 2인 양방향** — high 레이어 전달, 양쪽 영상 정상
4. **simulcast ON 3인** — 노트북 1대 한계 인지. enc_time 40ms+ 정상 (CPU 병목)
5. **레이어 전환** — long-press로 h→l→pause→h 전환. 각 전환 시 영상 복구 확인

**문제 발생 시**:
- 코드 안 고침
- 서버 디버그 로그 확인 (`[SIM:PLI]`, `[DBG:SIM]`, `SUBSCRIBE_LAYER`)
- 클라이언트 콘솔 로그 확인 (`[MEDIA]`, `[DBG:TRACK]`)
- 텔레메트리 스냅샷 분석
- 원인 확정 후에만 코딩

---

## Phase 2 (Phase 3 이후) — 고정 레이어 구독 (Rewrite 없이)

Phase 3가 안정되면, Phase 2는 선택적:
- IntersectionObserver 기반 visibility → layer 자동 선택
- Rewrite 없이 고정 레이어만 전달하는 경량 모드

---

## Phase 4 — Active Speaker 자동 전환 (미래)

- 서버: RFC 6464 audio level detection → ACTIVE_SPEAKERS broadcast (op=144)
- 클라이언트: active speaker → 메인 타일 승격 + h, 나머지 → l

---

## 이전 세션의 실수 목록 (반면교사)

| # | 실수 | 결과 | 교훈 |
|---|------|------|------|
| 1 | real SSRC를 그대로 사용하려 함 | subscriber SDP와 불일치 → 화면 안 나옴 | 처음부터 가상 SSRC 설계 |
| 2 | 가상 SSRC 도입 후 SSRC 참조 경로 일부만 수정 | PLI/NACK 역매핑 누락 → 키프레임 영원히 안 옴 | 체크리스트 10항목 한번에 수정 |
| 3 | ingress fallthrough에서 SSRC만 바꾸고 seq/ts 안 바꿈 | 나중에 rewriter offset 재계산 시 불연속 → jb_delay 폭등 | 처음부터 SubscribeLayer 즉석 생성 |
| 4 | SimulcastRewriter 초기화 시 P-frame 통과 | Chrome 디코딩 불가 → 플레이스홀더 | 항상 키프레임부터 시작 |
| 5 | PLI 교착 미인지 | 영상 영구 미표시 | PLI 경로 2중 보장 |
| 6 | `addTrack()` 사용 → direction sendrecv | 텔레메트리에서 방향 오보 | 항상 `addTransceiver(sendonly)` |
| 7 | joinRoom 중복 호출 미방어 | "already in room" 에러 반복 | SDK + UI 이중 가드 |
| 8 | `Arc<Room>`을 move 클로저에 캡처 | 컴파일 에러 (E0507) | `bool` Copy 타입으로 밖에서 캡처 |
| 9 | 추측으로 코드 수정 반복 | 핑퐁 → 시간 낭비 | **로그 먼저** |
| 10 | long-press 수동 설정이 redistributeTiles에 의해 덮어써짐 | 레이어 선택 안 먹힘 | manualOverride Set으로 보호 |

---

## 작업 진행 시 클로드에게 요구할 것

1. **코딩 전에 반드시 설계 + 변경 범위 + 체크리스트를 먼저 보여줘**
2. **한 Phase의 모든 변경을 한 턴에 완료해. "다음 턴에서" 금지**
3. **문제 발생 시 로그를 먼저 요청해. 추측 코딩 절대 금지**
4. **컴파일 에러 가능성 (Arc move, pub 접근성 등) 코딩 시점에 미리 처리해**
5. **빌드 성공 후 어떤 테스트를 어떤 순서로 해야 하는지 구체적으로 안내해**

---

*이 문서는 2026-03-19 세션의 모든 실패를 기반으로 작성됨.*
*"이전에 성공한 사례가 있는가?" — 이 질문을 항상 먼저 하자.*