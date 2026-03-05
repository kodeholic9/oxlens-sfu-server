Media Telemetry 지침서 v2

1. 목적
   우리 SFU가 WebRTC 프로토콜의 기대 계약을 제대로 이행하고 있는지 숫자로 증명한다. 추측이 아니라 측정 기반으로 품질 병목을 식별하고 튜닝 근거를 확보한다. 수집된 데이터는 Claude가 분석하므로 기계 파싱에 최적화된 구조로 출력한다.
2. 원칙

오염 방지: telemetry 데이터는 비즈니스 로직, 일반 로그, 콘솔 출력과 완전 분리한다
원본 보존: 가공/필터링 없이 원본 지표를 수집하고, 집계는 어드민에서 한다
부하 최소: hot path 성능에 영향을 주지 않는 수집 방식만 허용한다
단일 관측점: 모든 telemetry는 어드민 대시보드에서만 확인한다
Claude 친화: 모든 출력은 구조화된 텍스트로, 복사→붙여넣기→Claude 분석이 즉시 가능해야 한다

3. 수집 구간 정의
   미디어 품질은 4개 구간으로 분해한다.
   [구간 S] [구간 A] [구간 B] [구간 C]
   SDP/Codec 상태 Publisher 단말 → SFU 서버 → Subscriber 단말
   (협상 결과 검증) (인코더→전송) (수신→처리→전달) (수신→jitter buffer→디코더)
4. 수집 대상 상세
   4.1 구간 S — SDP 및 Codec 상태 (클라이언트 → 서버 보고)
   방 입장 시 1회 + re-negotiation 발생 시 추가 보고.
   S-1. SDP m-line 상태
   ID지표명설명수집 소스S-1-1pub_local_sdppublish PC의 local SDP 전문pc.localDescription.sdpS-1-2pub_remote_sdppublish PC의 remote SDP 전문pc.remoteDescription.sdpS-1-3sub_local_sdpsubscribe PC의 local SDP 전문pc.localDescription.sdpS-1-4sub_remote_sdpsubscribe PC의 remote SDP 전문pc.remoteDescription.sdpS-1-5pub_mline_summarypublish m-line별 요약 (아래 구조)SDP 파싱S-1-6sub_mline_summarysubscribe m-line별 요약 (아래 구조)SDP 파싱
   m-line 요약 구조 (m-line당 1개):
   {
   mid: "0",
   kind: "audio",
   direction: "sendonly",
   codec: "opus/48000/2",
   pt: 111,
   ssrc: 456672133,
   rtx_ssrc: null,
   fmtp: "minptime=10;useinbandfec=1;usedtx=1",
   rtcp_fb: ["nack"],
   extmap: [1, 4, 5, 6],
   ice_state: "connected",
   dtls_state: "connected",
   bundle_group: "BUNDLE 0 1"
   }
   S-2. 인코더/디코더 상태 (3초 주기 보고)
   ID지표명설명수집 소스S-2-1encoder_implementation브라우저가 선택한 인코더 (HW/SW)getStats: outbound-rtp .encoderImplementationS-2-2decoder_implementation브라우저가 선택한 디코더 (HW/SW)getStats: inbound-rtp .decoderImplementationS-2-3power_efficient_encoderHW 가속 사용 여부getStats: outbound-rtp .powerEfficientEncoderS-2-4codec_id실제 사용 중인 코덱 정보getStats: codec .mimeType, .clockRate, .channelsS-2-5encoder_quality_limitation인코더 품질 제한 사유getStats: outbound-rtp .qualityLimitationReasonS-2-6encoder_quality_durations제한 사유별 누적 시간getStats: outbound-rtp .qualityLimitationDurationsS-2-7frames_encoded인코딩 완료 프레임 수getStats: outbound-rtp .framesEncodedS-2-8key_frames_encoded키프레임 인코딩 수getStats: outbound-rtp .keyFramesEncodedS-2-9frames_decoded디코딩 완료 프레임 수getStats: inbound-rtp .framesDecodedS-2-10key_frames_decoded키프레임 디코딩 수getStats: inbound-rtp .keyFramesDecodedS-2-11frames_per_second_sent실제 송신 FPSgetStats: outbound-rtp .framesPerSecondS-2-12frames_per_second_received실제 수신 FPSgetStats: inbound-rtp .framesPerSecond
   4.2 구간 A — Publisher 단말 (클라이언트 → 서버 보고, 3초 주기)
   ID지표명설명수집 소스A-1outbound_packets_sent송신 RTP 패킷 수 (audio/video 분리)getStats: outbound-rtpA-2outbound_bytes_sent송신 바이트 수getStats: outbound-rtpA-3outbound_nack_count수신된 NACK 수getStats: outbound-rtpA-4outbound_pli_count수신된 PLI 수getStats: outbound-rtpA-5outbound_target_bitrate브라우저가 결정한 목표 비트레이트getStats: outbound-rtpA-6outbound_retransmitted_packets재전송 패킷 수getStats: outbound-rtpA-7candidate_pair_rttICE candidate pair 왕복 시간getStats: candidate-pairA-8candidate_pair_available_bitrate사용 가능 대역폭 추정치getStats: candidate-pair
   4.3 구간 B — SFU 서버 (서버 자체 계측, 3초 집계)
   ID지표명설명집계 방식B-1relay_duration_usdecrypt 시작 ~ 마지막 send_to 완료min/max/avg/p95/countB-2decrypt_duration_usSRTP decrypt 소요 시간min/max/avg/p95/countB-3encrypt_duration_usSRTP encrypt 소요 시간 (per target)min/max/avg/p95/countB-4lock_wait_usMutex 획득 대기 시간min/max/avg/p95/countB-5fan_out_countrelay 성공 참가자 수min/max/avgB-6encrypt_fail_countencrypt 실패 횟수sumB-7decrypt_fail_countdecrypt 실패 횟수sumB-8nack_received_count수신 NACK 수sumB-9rtx_sent_countRTX 재전송 수sumB-10rtx_cache_miss_countRTX 캐시 미스 수sumB-11pli_sent_countPLI 전송 수sumB-12rtcp_sr_relayed_countSR relay 수sumB-13rtcp_rr_relayed_countRR relay 수sumB-14twcc_feedback_sentTWCC feedback 수sum
   4.4 구간 C — Subscriber 단말 (클라이언트 → 서버 보고, 3초 주기)
   ID지표명설명수집 소스C-1inbound_packets_received수신 RTP 패킷 수getStats: inbound-rtpC-2inbound_packets_lost손실 패킷 수getStats: inbound-rtpC-3inbound_jitter수신 jitter (초)getStats: inbound-rtpC-4inbound_nack_sent보낸 NACK 수getStats: inbound-rtpC-5jitter_buffer_delayjitter buffer 누적 지연getStats: inbound-rtpC-6jitter_buffer_emittedJB에서 나온 샘플/프레임 수getStats: inbound-rtpC-7frames_decoded디코딩 완료 프레임 수getStats: inbound-rtpC-8frames_dropped드롭된 프레임 수getStats: inbound-rtpC-9freeze_count영상 프리즈 횟수getStats: inbound-rtpC-10total_freeze_duration총 프리즈 시간 (초)getStats: inbound-rtpC-11audio_concealment_events오디오 보정 횟수getStats: inbound-rtp
   C-5 ÷ C-6 = 평균 jitter buffer 지연 → 체감 지연의 핵심 지표
5. 데이터 전달 방식
   5.1 클라이언트 → 서버

전용 opcode 신설 (예: OP_TELEMETRY)
3초마다 S-2 + A + C 구간 지표를 JSON으로 서버에 전송
S-1(SDP 전문)은 방 입장 시 1회 + re-negotiation 시 추가 전송
기존 시그널링 WS 채널 공유, 서버는 비즈니스 로직에 사용하지 않음
서버는 수신 즉시 가공 없이 어드민 채널로 전달

5.2 서버 → 어드민

어드민 전용 WS 채널 (기존 어드민 WS 활용)
3초 주기로 B 구간 집계 + 클라이언트 telemetry 중계
room 단위 / participant 단위로 구분 전송

6. 어드민 표현 방식
   6.1 SDP 상태 패널
   방 선택 시 참가자별 SDP 상태를 구조화 테이블로 표시.
   m-line 상태 테이블:
   참가자PCmidkinddirectioncodecPTSSRCRTXfmtp 주요값ICEDTLSU214pub0audiosendonlyopus/48000/2111456672133-usedtx=1connectedconnectedU214pub1videosendonlyVP8/900009627289784832697598594-connectedconnectedU214sub0audiorecvonlyopus/48000/21114259169172--connectedconnectedU214sub1videorecvonlyVP8/900009612789517401278952740-connectedconnected
   이상 감지 강조 조건:

direction이 inactive → 빨강
ICE/DTLS가 connected가 아님 → 빨강
usedtx=1 → 노랑 (음질 영향 가능)
SSRC가 0 또는 null → 빨강

6.2 인코더/디코더 상태 패널
참가자방향kindcodec구현체HW가속FPS품질제한U214pubvideoVP8libvpxN24bandwidthU214pubaudioopusopus--noneU214sub←U100videoVP8libvpxN22-U214sub←U100audioopusopus---
6.3 실시간 개요 테이블
참가자방향패킷/s손실률jitter(ms)JB지연(ms)RTT(ms)SFU처리(ms)freezeconcealmentU214pub800%--12---U214sub←U100780.1%4.022121.203
이상 수치 강조 임계값: 손실률 >1%, jitter >30ms, JB지연 >100ms, SFU처리 >5ms, freeze >0, concealment 증가율 >1%
6.4 시계열 차트 (참가자 클릭 시)

차트 1: 구간별 지연 분해 (stacked area) — RTT/2 + SFU 처리시간 + JB 지연
차트 2: 패킷 흐름 (line) — 송신 pps vs 수신 pps vs 손실 pps
차트 3: 품질 지표 (line) — target bitrate, jitter, freeze count
차트 4: SFU 내부 (line) — lock_wait, decrypt_time, encrypt_time, relay_total

최근 5분 롤링 윈도우, 3초 간격.
6.5 WebRTC 계약 이행 체크리스트
항목판정 기준소스상태SDP 협상 정상모든 m-line direction이 기대값과 일치S-1✅/❌인코더 정상qualityLimitationReason == "none"S-2-5✅/❌디코더 정상framesDropped 증가율 < 1%S-2-9, C-8✅/❌SR relay 정상B-12 > 0 (3초당 최소 1회)B-12✅/❌RR relay 정상B-13 > 0B-13✅/❌NACK→RTX 응답B-9 / B-8 > 80%B-8, B-9, B-10✅/❌PLI→keyframe 전달A-4 증가 후 C-7 증가 확인A-4, S-2-10✅/❌TWCC feedbackB-14 > 0B-14✅/❌bitrate adaptationA-5 변동 존재A-5✅/❌jitter buffer 안정C-5÷C-6 < 100msC-5, C-6✅/❌영상 프리즈 없음C-9 == 0C-9✅/❌오디오 보정 최소C-11 증가율 < 1%C-11✅/❌FPS 일치S-2-11 ≈ S-2-12 (±20%)S-2-11, S-2-12✅/❌ 7. Claude 분석 친화 출력 규격
어드민에서 "스냅샷 내보내기" 버튼을 누르면 현재 상태를 아래 형식으로 클립보드에 복사한다. 이 텍스트를 Claude에 붙여넣으면 즉시 분석 가능해야 한다.
7.1 출력 형식

```
=== LIGHT-SFU TELEMETRY SNAPSHOT ===
timestamp: 2025-06-03T00:09:45.000Z
room_id: room_001
duration: 180s (since join)

--- SDP STATE ---
[U214:pub] mid=0 audio sendonly opus/48000/2 pt=111 ssrc=456672133 fmtp=minptime=10;useinbandfec=1;usedtx=1 ice=connected dtls=connected
[U214:pub] mid=1 video sendonly VP8/90000 pt=96 ssrc=2728978483 rtx=2697598594 ice=connected dtls=connected
[U214:sub] mid=0 audio recvonly opus/48000/2 pt=111 ssrc=4259169172 src=U100 ice=connected dtls=connected
[U214:sub] mid=1 video recvonly VP8/90000 pt=96 ssrc=1278951740 rtx=1278952740 src=U100 ice=connected dtls=connected
[U100:pub] mid=0 audio sendonly opus/48000/2 pt=111 ssrc=891234567 fmtp=minptime=10;useinbandfec=1;usedtx=1 ice=connected dtls=connected
[U100:pub] mid=1 video sendonly VP8/90000 pt=96 ssrc=123456789 rtx=123456790 ice=connected dtls=connected
[U100:sub] mid=0 audio recvonly opus/48000/2 pt=111 ssrc=456672133 src=U214 ice=connected dtls=connected
[U100:sub] mid=1 video recvonly VP8/90000 pt=96 ssrc=2728978483 rtx=2697598594 src=U214 ice=connected dtls=connected

--- ENCODER/DECODER ---
[U214:pub:video] impl=libvpx hw=N fps=24 quality_limit=bandwidth keyframes=3
[U214:pub:audio] impl=opus
[U214:sub:video:U100] impl=libvpx hw=N fps=22 keyframes_decoded=3
[U214:sub:audio:U100] impl=opus
[U100:pub:video] impl=libvpx hw=N fps=24 quality_limit=none keyframes=2
[U100:pub:audio] impl=opus

--- PUBLISH (3s window) ---
[U214:audio] pkts=150 bytes=6800 nack=0 pli=0 bitrate=18133 retx=0
[U214:video] pkts=72 bytes=95000 nack=2 pli=1 bitrate=253333 retx=2 fps=24
[U100:audio] pkts=150 bytes=7200 nack=0 pli=0 bitrate=19200 retx=0
[U100:video] pkts=75 bytes=102000 nack=1 pli=0 bitrate=272000 retx=1 fps=24

--- SUBSCRIBE (3s window) ---
[U214←U100:audio] pkts=148 lost=2 jitter=4.0ms jb_delay=22ms nack_sent=1 concealment=3
[U214←U100:video] pkts=73 lost=1 jitter=8.0ms jb_delay=35ms fps=22 freeze=0 dropped=0
[U100←U214:audio] pkts=150 lost=0 jitter=3.0ms jb_delay=18ms nack_sent=0 concealment=0
[U100←U214:video] pkts=71 lost=1 jitter=12.0ms jb_delay=42ms fps=23 freeze=0 dropped=0

--- NETWORK ---
[U214:pub] rtt=12ms available_bitrate=2500000
[U214:sub] rtt=12ms
[U100:pub] rtt=8ms available_bitrate=3200000
[U100:sub] rtt=8ms

--- SFU SERVER (3s window) ---
[U214] relay: avg=1.2ms max=3.8ms p95=2.1ms count=222
[U214] decrypt: avg=0.1ms max=0.3ms
[U214] encrypt: avg=0.2ms max=0.8ms
[U214] lock_wait: avg=0.05ms max=0.4ms
[U100] relay: avg=1.1ms max=2.9ms p95=1.8ms count=225
[U100] decrypt: avg=0.1ms max=0.2ms
[U100] encrypt: avg=0.2ms max=0.6ms
[U100] lock_wait: avg=0.04ms max=0.3ms
[server] nack_recv=3 rtx_sent=3 rtx_miss=0 pli_sent=1 sr_relay=6 rr_relay=4 twcc=0

--- CONTRACT CHECK ---
[PASS] sdp_negotiation: all m-lines match expected direction
[PASS] encoder_healthy: no quality limitation (U100), bandwidth limited (U214)
[PASS] sr_relay: 6 in 3s
[PASS] rr_relay: 4 in 3s
[PASS] nack_rtx: 3/3 = 100% hit rate
[PASS] pli_keyframe: pli=1 → keyframes_decoded increased
[FAIL] twcc_feedback: 0 sent (not implemented)
[WARN] bitrate_adaptation: U214 target_bitrate=253333 but available=2500000 (quality_limit=bandwidth)
[PASS] jitter_buffer: max jb_delay=42ms < 100ms
[PASS] video_freeze: 0 freezes
[PASS] audio_concealment: 3 events (< 1% of samples)
[WARN] usedtx: U214 audio fmtp contains usedtx=1
```

7.2 설계 근거

한 줄 = 한 사실: 파싱 용이, grep 가능
[주체:방향:kind:상대] 태그: 누구의 어떤 데이터인지 즉시 식별
단위 명시: ms, %, 개수 혼동 방지
PASS/FAIL/WARN: Claude가 즉시 문제 항목 식별 가능
SDP 전문 미포함: 요약만 포함, 전문이 필요하면 별도 "SDP 내보내기"로 분리
코드블록: Claude에 붙여넣을 때 마크다운 해석 방지

7.3 Claude 분석 요청 템플릿
어드민에 아래 텍스트를 "분석 요청" 버튼으로 함께 복사:
아래는 light-sfu WebRTC SFU 서버의 telemetry 스냅샷입니다.
다음을 분석해주세요:

1. WebRTC 프로토콜 계약 위반 항목과 영향
2. 음질/화질/지연 문제의 근본 원인 추정
3. 우선순위별 개선 권고
4. 구현 순서
   단계작업효과1단계클라이언트 SDP/codec 상태 수집 + 서버 경유 어드민 전달SDP 협상 검증2단계서버 B 구간 계측 + 어드민 pushSFU 내부 병목 가시화3단계클라이언트 telemetry 보고 (A+C)단말 상태 가시화4단계어드민 개요 테이블 + SDP 패널실시간 모니터링5단계어드민 시계열 차트추세 분석6단계Contract 체크리스트 + 스냅샷 내보내기Claude 분석 연동
