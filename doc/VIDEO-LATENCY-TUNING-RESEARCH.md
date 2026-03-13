# VIDEO-LATENCY-TUNING-RESEARCH

> WebRTC 영상 지연 최적화 — libwebrtc 공개 API/설정값 기반 튜닝 조사
> 작성: kodeholic (powered by Claude)
> 날짜: 2026-03-13

---

## 목적

비디오 페이즈 진입 시 적용할 WebRTC 영상 지연 최적화 기법을 사전 조사한다.
Google Meet, Amazon Chime, LiveKit, mediasoup 등 주요 플랫폼/SFU가 libwebrtc를
해체하지 않고 공개 API와 설정값만으로 조율하는 방법과 적정값을 정리한다.

---

## 1. 비트레이트 제어 (SDP Munging + Native API)

### 1.1 문제

libwebrtc 기본 시작 비트레이트는 **300kbps**. 목표 비트레이트까지 ramp-up에
수초~수십초 소요. 이 구간 동안 저화질 + 해상도 변동 발생.

### 1.2 SDP fmtp 파라미터 (Google 전용)

SDP answer의 `a=fmtp` 라인에 삽입:

```
a=fmtp:96 x-google-start-bitrate=1000;x-google-max-bitrate=2500;x-google-min-bitrate=100
```

| 파라미터 | 설명 | 기본값 |
|---|---|---|
| `x-google-start-bitrate` | BWE 초기 추정치 (kbps) | 300 |
| `x-google-max-bitrate` | 인코더 상한 캡 (kbps) | ~2500 (코덱 의존) |
| `x-google-min-bitrate` | 인코더 하한 (kbps) | 30 |

**주의사항:**
- Chromium 기반 + Safari 12+에서만 동작. Firefox는 미지원.
- `x-google-min-bitrate`를 높이면 혼잡 시에도 강제 전송 → 패킷 로스 악화 위험.
- discuss-webrtc에서 여러 개발자가 SDP munging 후에도 300kbps에서 안 올라가는
  현상 보고. 원인은 **SFU가 RTCP SR/RR을 제대로 반환하지 않아** BWE가 작동 안 함.

### 1.3 Native C++ API

```cpp
// PeerConnection 생성 후 호출
pc->SetBitrate(webrtc::BitrateSettings{
    .min_bitrate_bps = 100'000,    // 100 kbps
    .start_bitrate_bps = 1'000'000, // 1 Mbps
    .max_bitrate_bps = 2'500'000,   // 2.5 Mbps
});
```

SDP munging보다 안정적. livekit-webrtc 0.2.0에서 `PeerConnection`에 이 API가
노출되어 있는지 확인 필요.

### 1.4 프로덕션 참고값

| 시나리오 | start | max | min |
|---|---|---|---|
| 일반 회의 (720p) | 800~1000 kbps | 1500~2500 kbps | 100~200 kbps |
| 고품질 (1080p) | 1500 kbps | 4000 kbps | 300 kbps |
| Amazon Chime 표준 | 미공개 | 2500 kbps (720p) | 미공개 |
| Amazon Chime HD | 미공개 | 2500 kbps (1080p) | 미공개 |

### 1.5 RTCP 피드백 전제조건

BWE가 동작하려면 SFU가 다음을 반환해야 함:
- **RTCP Receiver Report (RR)** — 패킷 손실률, 지터 보고
- **TWCC 피드백** — 패킷 도착 시간 보고 (transport-cc 확장)

한 개발자 사례: RTCP 리포트를 활성화하자 SDP 설정 없이도 비트레이트가
300kbps 이상으로 올라감. **우리 서버의 `rtcp.rs`가 이것을 제대로 하고 있는지
비디오 페이즈 전 반드시 검증해야 함.**

---

## 2. Pacing / BWE Ramp-up

### 2.1 SetBitrate 오버프로비저닝

실전 팁: 10Mbps를 보내려면 SetBitrate를 30~50Mbps로 설정.
libwebrtc의 pacer가 데이터를 빨리 밀어내도록 유도.
Pacing Factor를 직접 조절하는 것과 유사한 효과.

### 2.2 Jitter Buffer 시작 지연

- libwebrtc 기본 `kStartDelayMs` = **80ms** (보수적)
- 30~40ms로 줄이면 초기 지연 감소, 단 언더런(underrun) 리스크 증가
- 적응적으로 변하므로 `kStartDelayMs`는 시작 시에만 영향

### 2.3 Field Trial 주입

libwebrtc는 런타임에 동작을 변경하는 Field Trial 메커니즘을 제공.

```cpp
// PeerConnectionFactory 생성 전에 호출 (프로세스 전역)
webrtc::field_trial::InitFieldTrialsFromString(
    "WebRTC-Video-Pacing/factor:3.0/"
);
```

형식: `<key>/<value>/` 쌍의 연결. 끝에 `/` 필수.

**주요 Field Trial:**

| Key | 값 예시 | 설명 |
|---|---|---|
| `WebRTC-Video-Pacing` | `factor:3.0` | Pacer 배수 상향 (기본 2.5) |
| `WebRTC-Pacer-DrainQueue` | `Enabled` | 큐 즉시 배출 |

**주의:** Field Trial은 Google 내부 A/B 테스트용. 버전 간 호환 보장 없음.
Facebook이 `WebRTC-Audio-OpusAvoidNoisePumpingDuringDtx` field trial로
오디오-비디오 간 지연이 초당 80ms씩 누적되는 문제를 겪은 사례 있음.
→ 적용 시 반드시 자체 테스트 필수.

---

## 3. Keyframe / PLI 관리

### 3.1 핵심 문제

새 수신자 구독 또는 화자 전환 시:
1. SFU → 송신자에게 PLI 전송
2. 송신자 인코더가 keyframe 생성
3. Keyframe 전송 + 수신 측 디코딩

이 과정에 **RTT + 인코딩 시간 = 1~2초** 블랙스크린 발생.

### 3.2 mediasoup: Paused Consumer 패턴

mediasoup의 권장 패턴:
1. Consumer를 `paused: true`로 생성
2. 수신 측에 consumer 파라미터 전송
3. 수신 측이 로컬 consumer 생성 완료 후 서버에 resume 요청
4. 서버가 resume → 이 시점에 PLI 요청

`paused: false`로 만들면 즉시 keyframe 요청하지만, 수신 측이 준비되기 전에
keyframe이 도착하면 손실 → 다시 PLI → 추가 지연.

### 3.3 대규모 PLI 폭풍 방지

1000명+ 규모에서 수신자 PLI/FIR 폭주 사례:
- 모든 수신 측 PLI를 SFU에서 차단 (terminate)
- 서버가 **3초 간격 주기적 FIR** 전송
- 우리 서버: PLI burst 메커니즘으로 이미 유사 패턴 적용 중

### 3.4 Keyframe 캐시 (서버 측)

제미나이 문서에서 언급된 "서버 측 I-Frame 캐싱 + 즉시 푸시":
- 각 publisher의 최신 keyframe 패킷 뭉치를 링 버퍼에 보관
- 새 수신자 구독 시 PLI 없이 즉시 캐시된 keyframe 전송
- **trade-off:** 참가자당 수십~수백KB GOP 캐시 필요 → RPi 메모리 제약 고려

---

## 4. Simulcast / SVC

### 4.1 Simulcast (LiveKit 방식)

- Publisher가 3개 해상도 레이어를 동시 인코딩하여 전송
- 추가 대역폭: ~17% (저해상도 레이어는 매우 작음)
- SFU가 수신자 대역폭에 따라 레이어 선택
- **단점:** 레이어 전환 시 keyframe 필요 → 순간 블랙스크린

### 4.2 SVC (Amazon Chime 방식)

- 단일 스트림에 3 spatial + 3 temporal 레이어 인코딩
- SFU가 레이어 단위로 드롭 가능 (keyframe 불필요)
- 전환이 simulcast보다 매끄러움
- **단점:** 코덱 지원 제한 (VP9/AV1), 인코더 복잡도 높음

### 4.3 우리 프로젝트 적용 판단

| 방식 | 장점 | 단점 | RPi 적합성 |
|---|---|---|---|
| Simulcast | 구현 단순, 널리 사용 | 레이어 전환 시 keyframe | 인코딩 부담 3배 |
| SVC | 매끄러운 전환 | 코덱 제한, 복잡 | VP9 HW 지원 필요 |
| 단일 스트림 | 최소 부담 | 적응 불가 | ✅ 최적 |

**결론:** RPi 타겟에서는 단일 스트림이 현실적. Simulcast은 데스크탑/모바일
클라이언트 환경에서 선택적 활성화.

---

## 5. 적용 로드맵

### Phase 현재 (오디오 PTT)

- 해당 없음 (비디오 미사용)
- 단, RTCP SR/RR 반환 정상 여부 선제 확인 권장

### Phase 다음 (비디오 추가 시)

| 우선순위 | 기법 | 작업 위치 | 난이도 |
|---|---|---|---|
| 1 | `SetBitrate(start=1000kbps)` | SDK `MediaSession` | 낮음 |
| 2 | SDP fmtp `x-google-start-bitrate` | SDK SDP 빌더 | 낮음 |
| 3 | RTCP SR/RR 반환 검증 | 서버 `rtcp.rs` | 중간 |
| 4 | Field Trial 주입 구조 | SDK `PeerConnectionFactory` | 낮음 |
| 5 | Paused consumer 패턴 | 서버 + SDK subscribe 경로 | 중간 |
| 6 | Keyframe 캐시 | 서버 media router | 높음 |

### Phase 이후 (최적화)

- Simulcast 지원 (데스크탑/모바일 클라이언트 한정)
- TWCC 기반 동적 비트레이트 조절 고도화
- `getStats()` 기반 클라이언트 메트릭 수집 → 서버 연동

---

## 참고 출처

- discuss-webrtc: bandwidth control, PLI interval, field trials
- webrtcHacks: bandwidth probing, SFU load testing, cascading SFU
- Amazon Chime SDK JS: SDP helper (`withStartBitrate`), WebRTC media docs
- mediasoup API docs: consumer paused pattern, keyframe request
- LiveKit blog: simulcast 개요
- webrtc.googlesource.com: field trials 공식 문서
- rtcbits.com: x-google-*-bitrate SDP 파라미터 실험

---

*author: kodeholic (powered by Claude)*
