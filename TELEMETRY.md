# Media Telemetry 지표 설명서

> OxLens SFU 서버의 미디어 품질 모니터링 시스템.
> 각 지표의 의미, 정상 범위, 이상 판별 기준을 설명한다.

---

## 1. 개요

미디어 품질은 4개 구간으로 분해하여 측정한다.

```
[구간 S]           [구간 A]           [구간 B]           [구간 C]
SDP/Codec 상태      Publisher 단말  →  SFU 서버      →   Subscriber 단말
(협상 결과 검증)     (인코더→전송)      (수신→처리→전달)    (수신→jitter buffer→디코더)
```

- **구간 S**: 연결 설정이 올바른지 (SDP, 코덱, ICE 상태)
- **구간 A**: 보내는 쪽이 정상적으로 인코딩하고 전송하는지
- **구간 B**: SFU 서버가 패킷을 빠르게 중계하는지
- **구간 C**: 받는 쪽이 패킷을 정상 수신하고 디코딩하는지

---

## 2. 구간 S — SDP 및 코덱 상태

방 입장 시 1회 + subscribe PC 변경 시 보고.

### S-1. SDP m-line 요약

각 PeerConnection(publish/subscribe)의 SDP에서 추출.

| 항목 | 설명 | 정상 | 이상 |
|------|------|------|------|
| `mid` | 미디어 라인 식별자 | 0, 1, 2... 순차 | null이면 SDP 파싱 실패 |
| `kind` | 미디어 종류 | `audio` 또는 `video` | — |
| `direction` | 방향 | publish=`sendonly`, subscribe=`recvonly` | `inactive`이면 미디어 안 흐름 |
| `codec` | 사용 코덱 | `opus/48000` (audio), `VP8/90000` (video) | 불일치 시 디코딩 실패 |
| `pt` | Payload Type | audio=111, video=96 | 서버 정책과 불일치 시 문제 |
| `ssrc` | 스트림 식별자 | publish=숫자, subscribe=null (recvonly) | publish에서 null이면 문제 |

### S-2. 인코더/디코더 상태 (3초 주기)

| 항목 | 설명 | 정상 | 주의 | 이상 |
|------|------|------|------|------|
| `encoderImpl` | 인코더 구현체 | `libvpx` (SW) | — | `?`이면 미확인 |
| `powerEfficient` | HW 가속 여부 | `true`=HW, `false`=SW | SW도 정상 | — |
| `qualityLimitReason` | 품질 제한 사유 | `none` | `cpu`=CPU 부족 | `bandwidth`=대역폭 부족 |
| `fps` | 실제 인코딩 FPS | 설정값과 ±20% | 설정값의 50% 미만 | 0이면 인코딩 중단 |
| `decoderImpl` | 디코더 구현체 | `libvpx` | — | `?`이면 아직 디코딩 전 |

**qualityLimitReason 해석:**
- `none`: 정상. 인코더가 자유롭게 동작
- `bandwidth`: 네트워크 대역폭 부족. Chrome BWE가 bitrate를 낮추라고 지시
- `cpu`: CPU 성능 부족. 해상도/FPS를 낮춰야 함
- `other`: 기타 (드문 경우)

---

## 3. 구간 A — Publisher 단말 (3초 주기)

내가 보내는 미디어 스트림의 상태. `getStats()` outbound-rtp + candidate-pair.

| 항목 | 설명 | 정상 범위 | 주의 | 이상 |
|------|------|-----------|------|------|
| `packetsSent` | 송신 RTP 패킷 수 (누적) | 지속 증가 | — | 증가 멈추면 전송 중단 |
| `bytesSent` | 송신 바이트 (누적) | 지속 증가 | — | — |
| `bitrate` | **실측 비트레이트** (3초 delta) | audio 20-40kbps, video 100kbps+ | video < 50kbps | 0이면 전송 중단 |
| `targetBitrate` | Chrome이 결정한 목표 비트레이트 | 설정 maxBitrate 이하 | < 100kbps | < 50kbps |
| `nackCount` | 수신된 NACK 수 | 0 | 간헐적 소량 | 지속 증가 = 패킷 손실 |
| `pliCount` | 수신된 PLI 수 | 입장 시 1-2회 | 간헐적 | 지속 증가 = 키프레임 손실 |
| `retransmittedPacketsSent` | RTX 재전송 수 | 0 또는 nack과 비례 | — | nack보다 적으면 캐시미스 |
| `framesEncoded` | 인코딩된 프레임 수 (누적) | 지속 증가 | — | — |
| `framesSent` | 전송된 프레임 수 (누적) | `framesEncoded`와 동일 | 갭 > 0 | 갭 > 5이면 인코더 병목 |
| `hugeFramesSent` | 큰 프레임 전송 수 (누적) | 0 또는 키프레임 수와 비례 | 급증 | PLI 직후 burst이면 악순환 |
| `totalEncodeTime` | 누적 인코딩 시간 (초) | — | — | ÷ framesEncoded > 16ms이면 실시간 부담 |
| `qualityLimitationDurations` | **3초 delta** (bw/cpu/none/other 각 초) | none ≈ 3.0 | bw > 0 | cpu > 0 |
| `framesPerSecond` | 실제 송신 FPS | 설정값 ±20% | 설정값 50% 미만 | 0 |
| `rtt` | ICE candidate pair RTT | < 50ms (내부망) | 50-200ms | > 200ms |
| `availableBitrate` | 추정 가용 대역폭 | > 500kbps | 100-500kbps | < 100kbps |

**신규 지표 해석:**

- **`framesEncoded - framesSent` 갭**: 인코딩은 했는데 전송 못한 프레임. 양수가 지속되면 pacer 또는 네트워크 병목.
- **`hugeFramesSent`**: Chrome이 "큰 프레임"으로 판정한 것 (보통 키프레임). PLI 응답 후 급증하면 PLI→키프레임→bandwidth limit 악순환 증거.
- **`totalEncodeTime / framesEncoded × 1000`**: 프레임당 평균 인코딩 시간(ms). 16ms 초과면 30fps 실시간 인코딩에 부담, 30ms 초과면 심각.
- **`qualityLimitationDurations` delta**: S-2의 누적값과 달리, 이번 3초 윈도우 동안 각 reason에 머문 시간. `bandwidth=2.1s`이면 3초 중 2.1초 동안 대역폭 부족 상태.

**bitrate vs targetBitrate:**
- `targetBitrate`: Chrome BWE가 "이만큼 보내라"고 지시하는 값
- `bitrate`: 실제로 3초간 보낸 바이트를 역산한 값
- target보다 actual이 현저히 낮으면: 인코더가 target만큼 비트를 쓸 콘텐츠가 없음 (정적 화면)
- target이 매우 낮으면 (< 50kbps): BWE가 대역폭을 과소 추정 → TWCC/REMB 피드백 확인

---

## 4. 구간 B — SFU 서버 (3초 집계)

서버 내부 처리 성능. hot path에서 `Instant::now()` 기반 측정.

### 타이밍 지표 (마이크로초 → 밀리초로 표시)

| 항목 | 설명 | 정상 | 주의 | 이상 |
|------|------|------|------|------|
| `relay` | 전체 릴레이 시간 (decrypt~마지막 send_to) | avg < 1ms | avg 1-5ms | avg > 5ms |
| `decrypt` | SRTP 복호화 시간 | avg < 0.5ms | avg 0.5-2ms | avg > 2ms |
| `encrypt` | SRTP 암호화 시간 (target당) | avg < 0.5ms | avg 0.5-2ms | avg > 2ms |
| `lock_wait` | Mutex 획득 대기 시간 | avg < 0.1ms | avg 0.1-1ms | avg > 1ms (경합 발생) |

**해석 팁:**
- `relay` ≈ `decrypt` + (`encrypt` × fan_out) + network I/O
- `lock_wait`가 높으면: 동시 접근 경합. 참가자 수 증가 시 확인
- 라즈베리파이에서는 전체적으로 2-3배 느릴 수 있음

### 카운터 지표 (3초 윈도우)

| 항목 | 설명 | 정상 | 주의 | 이상 |
|------|------|------|------|------|
| `fan_out` | 릴레이 대상 참가자 수 | 참가자수-1 | — | 0이면 아무도 안 받음 |
| `nack_received` | 수신 NACK 수 | 0 | 간헐적 소량 | 지속적 = 패킷 손실 |
| `rtx_sent` | RTX 재전송 수 | nack과 비례 | — | nack보다 적으면 캐시미스 |
| `rtx_cache_miss` | RTX 캐시 미스 수 | 0 | — | > 0이면 캐시 크기 부족 or 지연 |
| `pli_sent` | PLI 전송 수 | 입장 시 1-2회 | 간헐적 | 지속적 = 키프레임 문제 |
| `sr_relayed` | SR(Sender Report) 릴레이 수 | > 0 (3초당 2-3회) | — | 0이면 RTCP 경로 문제 |
| `rr_relayed` | RR(Receiver Report) 릴레이 수 | > 0 | — | 0이면 subscribe RTCP 경로 문제 |
| `encrypt_fail` | 암호화 실패 | 0 | — | > 0이면 SRTP 키 문제 |
| `decrypt_fail` | 복호화 실패 | 0 | — | > 0이면 SRTP 키 불일치 |

**NACK → RTX 히트율:**
- `rtx_sent / (rtx_sent + rtx_cache_miss)` × 100%
- 정상: > 80%
- 낮으면: RTP 캐시 크기(128) 부족. 패킷이 캐시에서 이미 밀려남

---

## 5. 구간 C — Subscriber 단말 (3초 주기)

내가 받는 미디어 스트림의 상태. `getStats()` inbound-rtp + candidate-pair.

| 항목 | 설명 | 정상 | 주의 | 이상 |
|------|------|------|------|------|
| `sourceUser` | 이 트랙의 원본 발행자 | user_id 표시 | null이면 미확인 SSRC | — |
| `packetsReceived` | 수신 RTP 패킷 수 (누적) | 지속 증가 | — | 증가 멈추면 수신 중단 |
| `packetsLost` | 손실 패킷 수 (누적) | 0 | — | > 0이면 네트워크 손실 |
| `bitrate` | **실측 수신 비트레이트** (3초 delta) | pub bitrate와 유사 | 차이 > 30% | 0이면 수신 중단 |
| `jitter` | 수신 jitter (초 → ms 변환) | < 10ms | 10-30ms | > 30ms |
| `jitterBufferDelay` | JB 3초 delta 평균 지연 (ms) | < 50ms | 50-100ms | > 100ms |
| `framesDecoded` | 디코딩 완료 프레임 (누적) | 지속 증가 | — | — |
| `keyFramesDecoded` | 키프레임 디코딩 수 | 입장 시 1+ | — | 0이면 영상 안 나옴 |
| `framesDropped` | 드롭된 프레임 (누적) | 0 | 간헐적 | 지속 증가 |
| `framesPerSecond` | 실제 수신 FPS | pub FPS와 ±20% | 50% 미만 | 0 |
| `freezeCount` | 영상 프리즈 횟수 (누적) | 0 | 1-2회 | 지속 증가 |
| `totalFreezesDuration` | 총 프리즈 시간 (초) | 0 | < 1s | > 1s |
| `concealedSamples` | 오디오 보정(보간) 횟수 | 0 또는 낮은 값 | — | 급격히 증가 |
| `rtt` | subscribe PC RTT | < 50ms | 50-200ms | > 200ms |
| `nackCount` | 보낸 NACK 수 | 0 | 간헐적 | 지속적 |

**손실률 계산:**
```
loss% = packetsLost / (packetsReceived + packetsLost) × 100
```
- 정상: < 0.5%
- 주의: 0.5-1%
- 이상: > 1%

---

## 6. 네트워크 지표

| 항목 | 설명 | 정상 | 주의 | 이상 |
|------|------|------|------|------|
| `rtt` | 왕복 지연 시간 | < 50ms (내부망), < 100ms (외부) | 100-200ms | > 200ms |
| `availableBitrate` | Chrome BWE 추정 가용 대역폭 | > 1Mbps | 100kbps-1Mbps | < 100kbps |

**availableBitrate가 비정상적으로 낮은 경우 (예: 84kbps):**
- TWCC feedback이 구현되지 않으면 Chrome BWE가 초기 추정값에서 벗어나지 못함
- REMB feedback도 서버에서 생성하지 않으면 동일한 현상
- 실제 네트워크 대역폭과 무관하게 낮게 고정됨

---

## 7. 구간별 손실 Cross-Reference

어드민에서 publisher A의 `packetsSent`와 subscriber B의 `packetsReceived + packetsLost`를 매칭하여 구간별 손실을 추정한다.

```
A구간 (pub→SFU) 추정 손실 = pub.packetsSent - sub.(packetsReceived + packetsLost)
B→C구간 (SFU→sub) 확정 손실 = sub.packetsLost
```

**한계:**
- 누적값 기반이라 타이밍 차이로 음수가 나올 수 있음 (0 clamp)
- 서버에 SSRC별 수신 카운터가 없어 A→B와 B→C를 정확히 분리 불가
- 추세 파악용으로 사용하고, 정밀 진단에는 서버 SSRC별 카운터 추가 필요

**어드민 표시:**
- 참가자 상세 패널 하단 "구간별 손실 추정" 섹션
- `A→B:~N B→C:N sent:N` 형태로 표시

**스냅샷 포맷:**
```
[pub_user→sub_user:kind] pub_sent=N sub_recv=N sub_lost=N transit_loss≈N
```

---

## 8. 이벤트 타임라인

3초 주기 `getStats()` 폴링에서 이전값과 diff를 감지하여 상태 전이를 기록한다.
인과관계 규명의 핵심 — "PLI가 먼저냐, bandwidth limit이 먼저냐"를 시간순으로 판별 가능.

### 감시 이벤트 (10종)

**Publish (outbound-rtp):**

| 이벤트 | 트리거 조건 | 의미 |
|--------|------------|------|
| `quality_limit_change` | `qualityLimitationReason` 값 변화 | 인코더 품질 제한 상태 전이 |
| `encoder_impl_change` | `encoderImplementation` 값 변화 | HW↔SW fallback 감지 |
| `pli_burst` | 3초간 PLI 3개 이상 | 키프레임 요청 폭주 |
| `nack_burst` | 3초간 NACK 10개 이상 | 패킷 손실 폭주 |
| `bitrate_drop` | `targetBitrate`가 이전의 50% 이하로 하락 | BWE 급락 |
| `fps_zero` | `framesPerSecond`가 양수 → 0 | 인코딩/전송 중단 |

**Subscribe (inbound-rtp):**

| 이벤트 | 트리거 조건 | 의미 |
|--------|------------|------|
| `video_freeze` | `freezeCount` 증가 | 영상 프리즈 발생 |
| `loss_burst` | 3초간 `packetsLost` 20개 이상 증가 | 대량 패킷 손실 |
| `frames_dropped_burst` | 3초간 `framesDropped` 5개 이상 증가 | 프레임 드롭 폭주 |
| `decoder_impl_change` | `decoderImplementation` 값 변화 | 디코더 전환 |

### 링버퍼
- 클라이언트 SDK: 최근 50개 이벤트 보관 (`_eventLog`)
- 어드민: user_id별 최근 50개 이벤트 보관 (`eventHistory`)
- 3초 stats 보고에 `events` 배열로 이번 tick 이벤트 포함

### 어드민 표시
- 참가자 상세 패널 하단 "이벤트 타임라인" 섹션
- 최신순, 아이콘 + 색상(노랑=경고, 빨강=위험) + 시간 + 설명
- 최대 높이 제한 + 스크롤

### 스냅샷 포맷
```
--- EVENT TIMELINE ---
[user_id] 2026-03-11T14:30:00.000Z quality_limit_change pub:video quality none→bandwidth
[user_id] 2026-03-11T14:30:03.000Z pli_burst pub:video PLI burst ×4
```

---

## 9. Contract 체크리스트

WebRTC 프로토콜의 기대 계약이 이행되는지 자동 판정.

| 항목 | 판정 기준 | PASS | FAIL |
|------|-----------|------|------|
| `sdp_negotiation` | 모든 m-line direction이 기대값 | 정상 협상 | inactive 존재 |
| `encoder_healthy` | qualityLimitReason == "none" | 인코더 자유 | 품질 제한 발생 |
| `encoder_bottleneck` | framesEncoded - framesSent ≤ 5 | no gap | gap=N |
| `sr_relay` | SR relay > 0 (3초당) | RTCP SR 정상 중계 | SR 경로 단절 |
| `rr_relay` | RR relay > 0 (3초당) | RTCP RR 정상 중계 | RR 경로 단절 |
| `nack_rtx` | RTX hit rate > 80% | 재전송 정상 | 캐시 미스 과다 |
| `jitter_buffer` | JB 평균 지연 < 100ms | 버퍼 안정 | 과도한 버퍼링 |
| `video_freeze` | freezeCount == 0 | 프리즈 없음 | 프리즈 발생 |
| `bwe_feedback` | TWCC 또는 REMB feedback > 0 | 피드백 정상 | 피드백 없음 |
| `track_health` | 모든 track readyState == "live" | 정상 | ended 감지 |
| `pc_connection` | pubPc connectionState == "connected" | 정상 | failed/closed |
| `runtime_busy` | Tokio busy ratio < 85% | 정상 | > 85% HIGH, > 95% SATURATED |

---

## 10. 스냅샷 포맷

어드민에서 "📋 스냅샷" 버튼을 누르면 현재 상태를 텍스트로 클립보드에 복사.
이 텍스트를 Claude에 붙여넣으면 즉시 분석 가능.

### 포맷 규칙
- 한 줄 = 한 사실 (파싱 용이, grep 가능)
- `[주체:방향:kind:상대]` 태그로 누구의 어떤 데이터인지 즉시 식별
- 단위 명시 (ms, %, kbps)
- `PASS/FAIL/WARN` 태그로 문제 항목 즉시 식별

### 섹션 구성
```
=== OXLENS-SFU TELEMETRY SNAPSHOT ===
--- SDP STATE ---              ← 구간 S-1
--- ENCODER/DECODER ---        ← 구간 S-2
--- PUBLISH (3s window) ---    ← 구간 A (인코더 진단 라인 포함)
--- SUBSCRIBE (3s window) ---  ← 구간 C (sourceUser 포함)
--- NETWORK ---                ← 네트워크
--- LOSS CROSS-REFERENCE ---   ← 구간별 손실 추정 (신규)
--- EVENT TIMELINE ---         ← 이벤트 타임라인 (신규)
--- PTT DIAGNOSTICS ---        ← PTT 진단
--- SFU SERVER (3s window) --- ← 구간 B
--- CONTRACT CHECK ---         ← 계약 이행 판정
```

### PUBLISH 인코더 진단 라인 (video only)
```
[user:video:enc] encoded=1234 sent=1230 gap=4 huge=2 enc_time=8.3ms/f qld_delta=[bw=1.2s cpu=0.0s]
```

### Claude 분석 요청 템플릿
```
아래는 OxLens SFU WebRTC 서버의 telemetry 스냅샷입니다.
다음을 분석해주세요:
1. WebRTC 프로토콜 계약 위반 항목과 영향
2. 음질/화질/지연 문제의 근본 원인 추정
3. 이벤트 타임라인에서 인과관계 (닭-달걀) 규명
4. 구간별 손실 분포에서 병목 위치 특정
5. 우선순위별 개선 권고
```

---

## 11. 데이터 흐름

```
[클라이언트 SDK]                    [SFU 서버]                    [어드민]
    │                                   │                           │
    │── OP_TELEMETRY(30) ──────────────→│                           │
    │   section: "sdp"                  │── admin_tx.send() ───────→│ snapshot (접속 시)
    │   section: "stats" (3초)          │   type: "client_telemetry" │ client_telemetry (중계)
    │     + events[] (이벤트 타임라인)   │                           │ server_metrics (3초)
    │                                   │── flush_metrics() ───────→│ snapshot (join/leave)
    │                                   │   type: "server_metrics"  │
```

- 서버는 클라이언트 telemetry를 가공 없이 어드민으로 passthrough
- 서버 자체 지표(B구간)는 3초마다 집계 후 admin_tx로 push
- Room 변경(join/leave/cleanup) 시 rooms snapshot을 admin_tx로 push
- 이벤트 타임라인은 클라이언트에서 감지 → stats 보고에 events 배열로 포함 → 어드민 누적

---

## 12. 현재 구현 상태 (v0.6.4)

| 구간 | 수집 | 어드민 표시 | 비고 |
|------|------|------------|------|
| S-1 (SDP) | ✅ | ✅ | 입장 시 + subscribe 변경 시 |
| S-2 (코덱) | ✅ | ✅ | 3초 주기 |
| A (Publish) | ✅ | ✅ | delta bitrate + 인코더 진단 (framesSent/huge/encTime/qld delta) |
| B (SFU) | ✅ | ✅ | 타이밍 + RTCP 카운터 + PipelineStats + AggLogger |
| C (Subscribe) | ✅ | ✅ | sourceUser + delta bitrate 포함 |
| 구간별 손실 | ✅ | ✅ | pub→sub cross-reference (어드민 매칭) |
| 이벤트 타임라인 | ✅ | ✅ | 10종 상태 전이 감지, 링버퍼 50개 |
| Contract | ✅ | ✅ | 12항목 (encoder_bottleneck 포함) |
| 스냅샷 | ✅ | ✅ | LOSS CROSS-REF + EVENT TIMELINE + PIPELINE STATS 섹션 |
| PipelineStats | ✅ | ✅ | per-participant 파이프라인 카운터 (pub/sub 7종) |
| AggLogger | ✅ | ✅ | 핫패스 로그 집계 (DashMap 해시키 기반) |
| PLI/NACK 계측 | ✅ | ✅ | per-participant PLI·NACK·RTX 카운터 |
| Power State | ✅ | ✅ | powerStats 버킷 + ptt_power_change 이벤트 (클라이언트) |
| 시계열 차트 | 데이터 수집만 | ❌ | 향후 추가 예정 |

### 미구현 (향후)
- 시계열 차트 (어드민)
- REST API (`GET /admin/rooms`)
