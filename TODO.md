# TODO — 다음 작업 목록

## 🔜 oxlens-sdk-core (Android 네이티브 SDK)

### 환경 구축

- [x] WSL2 + Ubuntu 22.04 설치 — 2026-03-09
- [x] depot_tools 설치 — 2026-03-09
- [x] WebRTC Android 소스 fetch + gclient sync — 2026-03-09
- [x] install-build-deps 완료 — 2026-03-09
- [ ] libwebrtc arm64 첫 빌드 완료 (gn gen + ninja -j4)
- [ ] libwebrtc.aar 생성 확인
- [ ] Android Studio 설치 + SDK/NDK 셋업
- [ ] oxlens-sdk-core 프로젝트 생성 (Kotlin, min API 28)
- [ ] libwebrtc.aar 통합 + Gradle sync 확인

### Rx 파이프라인 (수신)

- [ ] C++ Interceptor: Dependencies 주입용 Proxy 클래스 작성
- [ ] Opus silence frame 주입기 (NetEQ Hot 유지)
- [ ] Sequence/Timestamp offset 보정 (서버 릴레이 로직 포팅)
- [ ] SSRC 기반 Audio/Video 패킷 분류

### Tx 파이프라인 (송신 FSM)

- [ ] Stage 1 (Soft-Mute): `encoding.active = false`, 하드웨어 ON
- [ ] Stage 2 (Hard-Mute): 하드웨어 OFF, Track 유지, "비디오 중단" 시그널
- [ ] Stage 3 (Deep Sleep): ICE Ping 장주기 전환
- [ ] Stage 전환 타임아웃 config 상수화 (1분, 10분)

### ICE Keep-alive 최적화

- [ ] `ice_candidate_pair_ping_interval` 동적 조정 (Stage별 10s/15s/30s+)
- [ ] 한국 통신사(SKT/KT/LGU+) LTE/5G NAT 타임아웃 실측

### 카메라 웜업 & 키프레임

- [ ] Android `onFirstFrameAvailable()` 콜백 연동
- [ ] 클라이언트 → 서버: CAMERA_READY 시그널 (opcode 추가)
- [ ] 서버: CAMERA_READY 수신 → PLI 2발 (즉시 + 150ms)

### MBCP 클라이언트 (네이티브 Floor Control)

- [ ] RTCP APP 패킷 빌더 (FREQ/FREL/FPNG) — C++ 또는 Kotlin/JNI
- [ ] RTCP APP 패킷 파서 (FTKN/FIDL/FRVK) — subscribe PC에서 수신
- [ ] PTT 버튼 UI → MBCP FREQ/FREL 전송
- [ ] sfu-labs bench 클라이언트에 MBCP 송수신 추가 (서버 검증용)

### 서버 추가 작업

- [ ] opcode 추가: CAMERA_READY, VIDEO_SUSPENDED, VIDEO_RESUMED
- [ ] VIDEO_SUSPENDED/RESUMED 핸들러 → 수신자 UI placeholder 전환 브로드캐스트
- [ ] 시그널링 단절 시 Full Cold Start 정책 구현

### 시그널링 단절 예외 처리

- [ ] 시그널링(TCP) 단절 감지 → 모든 캐시(SDP/DTLS/ICE) 파기
- [ ] Full Cold Start (신규 SDP 교환) 수행
- [ ] 좀비 세션 원천 차단 검증

---

## Backlog

- [ ] IDENTIFY token verification (JWT or shared secret)
- [ ] 서버 SSRC별 수신 패킷 카운터 (정밀 구간 손실 분리)
- [ ] VP8 키프레임 캐시 (서버, 화자 전환 시 즉시 주입)
- [ ] PTT 오디오 지연 원인 분석 및 최적화
- [ ] x86 서버 벤치마크 (50인+ 목표)
- [ ] Simulcast / SVC
- [ ] TURN relay support
- [ ] Recording (RTP → file)
- [ ] Data channel support
- [ ] Horizontal scaling (multi-node)
- [ ] 시계열 차트 (어드민)
- [ ] REST API (GET /admin/rooms)
- [ ] 타우리 데스크톱: WebView 내장 WebRTC vs 네이티브 libwebrtc 결정
