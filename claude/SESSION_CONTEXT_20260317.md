# SESSION_CONTEXT_20260317 — RTCP Terminator v1 + SR Translation

## 세션 목표
1. RTCP Terminator v1 구현 + 검증 (이전 세션 완료)
2. SR Translation 구현 + 검증
3. 체감 지연 분석
4. PTT 모드 통합 테스트

**전부 완료.**

## 완료된 작업

### 설계 (대화 기반)
- `getStats()` loss 54%의 원인 분석: 실제 loss vs gate 차단 vs 탭 hidden 구분 불가
- RR/SR → getStats() 매핑, NetEQ/BWE 반응 차이 분석
- PTT rewriter가 SSRC/seq/ts를 리라이트하는데 RTCP는 안 건드리는 문제 발견
- subscriber 9명의 RR을 publisher에게 릴레이하는 것 자체가 구조적으로 성립 안 됨
- **결론**: SFU가 peer로서 RR을 직접 생성해야 함 (RFC 8079, mediasoup/Janus/Pion 선례 확인)

### 구현
| 파일 | 변경 |
|------|------|
| `config.rs` | RTCP_REPORT_INTERVAL_MS, CLOCK_RATE_*, RTP_SEQ_* 상수 |
| `rtcp_terminator.rs` | **신규** — RecvStats(RFC 3550 A.3/A.8), SendStats, RR/SR 빌더, SR 파서, 단위테스트 |
| `participant.rs` | `recv_stats: Mutex<HashMap<u32, RecvStats>>`, `send_stats` 추가 |
| `rtcp.rs` | `split_compound_rtcp` — SR→`sr_blocks`, RR→`rr_blocks` 분리 |
| `ingress.rs` | hot path recv_stats 갱신 (RTX PT=97 제외), publisher SR 소비+릴레이, subscriber RR 소비 |
| `egress.rs` | `send_rtcp_reports()` — 1초 주기 RR 생성/전송, SR 자체생성 비활성 |
| `mod.rs` | `pub(crate) mod rtcp_terminator`, `rtcp_report_timer` 추가 |
| `metrics/mod.rs` | `rr_consumed`, `rr_generated`, `sr_generated` 카운터, `rr_diag` 진단 스냅샷 |

### 검증 결과
- **서버 자체 RR**: 동작 확인. frac_lost=0, cum_lost 정상, jitter 정상 범위
- **subscriber RR 차단**: rr_relay=0 확인, publisher BWE에 subscriber loss 전파 안 됨
- **SR 릴레이**: sr_relay=8~14/3s 정상 동작
- **RTX 유령 SSRC**: PT=97 필터링으로 제거. blocks=2(audio+video)로 정상화
- **CPU 병목 식별**: 노트북 1대 크롬 3개 → A→SFU 14~18% loss는 CPU throttling.
  2개로 줄이면 0%, RTCP Terminator 무관 확인

### 발견된 이슈 (수정 완료)
1. **RTX SSRC가 RR/SR에 포함** → jitter 억 단위 폭등. PT=97 필터링으로 해결
2. **SR 자체 생성이 jb_delay 폭등 유발** → 서버 자체 클록의 NTP/RTP가 원본 미디어와 불일치.
   Janus PR #2007과 동일 문제. SR 자체생성 비활성 + publisher SR 릴레이로 복원

## 미완료 (다음 세션)

### 높은 우선순위
- **SR translation 구현 ✅**: publisher SR → subscriber SR 변환 완료
  - `ptt_rewriter.rs`: `translate_rtp_ts()` 메서드 추가 (origin/virtual base 오프셋 연산)
  - `rtcp_terminator.rs`: `SrTranslation` + `translate_sr()` 함수 추가 + 단위테스트
  - `egress.rs`: `SendStats::on_rtp_sent()` 활성화 (RTX PT=97 제외, 헤더 길이 정확 계산)
  - `ingress.rs`: `relay_publish_rtcp()` → `relay_publish_rtcp_translated()` 리팩터링
    - subscriber별 SR 변환: `build_sr_translation()`이 PTT/Conference 모드 분기
    - PTT: SSRC→가상, RTP ts→오프셋 변환, counts→egress SendStats
    - Conference: SSRC/ts 원본 유지, counts만 egress 기준
- **검증 완료**:
  - Conference: sr_relay=16/3s, jb_delay 68~99ms, loss 0%, freeze 0 ✔
  - PTT: sr_relay 정상, 에러 없음 ✔
  - PTT 통합 테스트: frac_lost=0 전 구간, BWE 안정(~1.9Mbps), gate 전환 시 RR 폭등 없음 ✔
- **contract check 기준 업데이트 ✅**: `rr_relay` → `rr_generated` (render-panels.js)
- **진단 로그 정리 ✅**: rr_diag info → debug (mod.rs)

### 낮은 우선순위 (Backlog)
- **A/B 비교 검증**: 별도 머신 2대에서 Terminator ON/OFF 비교 (노트북 1대로는 CPU 변수 제거 불가)
- **RTCP report 주기 동적화**: config.rs 상수로 분리 완료, 추후 RFC 3550 Section 6.2 기반 계산 가능
- **PTT NACK 역매핑 last_speaker fallback ✅**: FloorController에 `prev_speaker` 필드 + `last_speaker()` 메서드 추가.
  ingress.rs의 NACK/PLI 역매핑에서 `current_speaker().or_else(last_speaker)` fallback 적용.
  release/revoke/leave 시 prev_speaker 저장, 다음 granted 시 초기화.
- **PTT NACK 잔여 문제 (TODO)**: 방 재진입(새로고침 없이) 시 `pub_not_found=15, nack_remap=0` 발생.
  NACK의 media_ssrc가 새 가상 SSRC와 매칭 안 됨 → last_speaker fallback 이전 단계에서 탈락.
  subscriber가 어떤 SSRC로 NACK을 보내는지 로그 확인 필요 (handle_nack_block에서 media_ssrc 기록).
- **PTT jb_delay 점진적 폭등 해결 ✅**: PTT 모드에서 SR 릴레이 중단.
  원인: 화자 교대 시 NTP는 idle 구간만큼 점프, RTP는 연속 스트림으로 거의 안 점프
  → NTP↔RTP 선형 관계 파괴 → Chrome jb가 버퍼 키움 → 25→420→837ms 누적
  PTT에서는 1인 발화라 lip sync 불필요, arrival time 기반으로 충분.
  Conference에서만 SR translation 유지.
- **PTT 화자 교대 시 남은 문제 (Backlog)**:
  - video freeze + loss burst: 키프레임 대기 → PLI 지연 사이 P-frame 디코딩 실패.

## 핵심 학습

### RTCP Terminator 설계 원칙
- SFU는 두 개의 독립된 RTP 세션의 종단점(peer)
  - Ingress: Publisher → SFU (서버가 수신자 → RR 생성)
  - Egress: SFU → Subscriber (서버가 송신자 → SR 필요)
- subscriber RR은 publisher에게 릴레이하지 않음
- publisher SR은 subscriber에게 릴레이 (자체 생성은 NTP 불일치 문제)

### SR 자체 생성의 함정
- SFU는 미디어 소스가 아니므로 자체 클록으로 SR을 만들면 NTP↔RTP 관계가 깨짐
- Janus에서도 동일 이슈 경험 (PR #2007)
- 해결: publisher SR 기반 translation 구현 완료

### SR Translation 설계 원칙
- NTP timestamp은 항상 원본 유지 (lip sync 기준점, 변조 금지)
- Conference: SSRC/RTP ts 원본, packet_count/octet_count만 egress SendStats 기준
- PTT: SSRC → virtual_ssrc, RTP ts → PttRewriter.translate_rtp_ts() 오프셋 변환
- translate_rtp_ts()가 None 반환 시(화자 없음/첫패킷 미도착) RTP ts는 원본 유지
- SendStats는 subscriber별 egress task에서 per-SSRC로 갱신 (participant.send_stats)

### RTX SSRC 필터링 필수
- publisher가 서버에 RTX(PT=97)를 보내는 경우 간헐적 → jitter 억 단위 폭등
- recv_stats/send_stats에서 PT=97은 반드시 제외

### 테스트 환경 한계
- 노트북 1대 크롬 3개: CPU 50~80%, A→SFU loss 14~18% 발생 → RTCP 문제와 혼동 위험
- 최소 2대 머신 필요, 또는 크롬 2개까지가 한계

## 현재 서버 상태
- v0.5.3 + RTCP Terminator v1 + SR Translation + NACK fallback + PTT SR 중단
- 서버 자체 RR 생성 ✅
- SR translation ✅ (Conference만, subscriber별 SSRC/ts/counts 변환)
- PTT SR 릴레이 중단 ✅ (NTP↔RTP drift → jb_delay 폭등 방지)
- SendStats egress 갱신 ✅ (RTX 제외)
- subscriber RR 소비 ✅
- RTX 필터링 ✅
- PTT NACK/PLI 역매핑 last_speaker fallback ✅
- contract check rr_generated 기준 ✅
- rr_diag debug 레벨 ✅
- rate-limited warn 진단 로그 ✅ (3초 윈도우 첫 건: NACK:DIAG, EGRESS:DIAG)

## 검증 결과 (PTT SR 중단 후)
- jb_delay video: **8ms** (SR 중단 전 837ms → 8ms)
- jb_delay 누적 폭등: 없음 (화자 7~8회 교대 후에도 안정)
- sr_relay=0: PTT에서 의도된 동작 (contract check 기준 조정 필요 - TODO)
- 지연 체감: 전혀 안 느껴짐 ✔
