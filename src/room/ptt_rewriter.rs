// author: kodeholic (powered by Claude)
//! PTT SSRC Rewriter — 화자 교대 시 가상 SSRC/seq/ts로 리라이팅
//!
//! 무전기 모드에서 여러 화자의 RTP를 하나의 연속 스트림으로 통합.
//! 수신 측 Jitter Buffer가 단일 SSRC 스트림으로 인식하도록
//! 서버가 가상 SSRC + seq/ts 오프셋 연산을 수행한다.
//!
//! 기술타당성검토서 §2.3 오프셋 기반 상대 연산:
//!   virtual_seq = original_seq - origin_base_seq + virtual_base_seq
//!   virtual_ts  = original_ts  - origin_base_ts  + virtual_base_ts
//!
//! 패킷이 비순차 도착(UDP)해도 원본의 상대적 순서가 보존된다.
//!
//! Audio: ts_guard_gap=960 (Opus 48kHz, 20ms), 키프레임 대기 없음
//! Video: ts_guard_gap=3000 (VP8 90kHz, ~33ms), 키프레임 대기 있음

use std::sync::Mutex;
use std::time::Instant;
use tracing::{debug, info};

/// rewrite() 결과
#[derive(Debug, PartialEq, Eq)]
pub enum RewriteResult {
    /// 리라이팅 성공 — fan-out 대상
    Ok,
    /// 키프레임 대기 중 — 이 패킷은 드롭
    PendingKeyframe,
    /// 리라이팅 대상 아님 (화자 불일치 등)
    Skip,
}

/// Audio/Video 각각 1개씩 Room에 소속
pub struct PttRewriter {
    /// 가상 SSRC (Room 생성 시 랜덤 할당, 고정)
    virtual_ssrc: u32,
    /// 화자 전환 시 가상 timestamp 간격 (Audio: 960, Video: 3000)
    ts_guard_gap: u32,
    /// 비디오 전용: 키프레임 대기 모드 사용 여부
    require_keyframe: bool,
    /// 내부 상태
    state: Mutex<RewriteState>,
}

struct RewriteState {
    /// 현재 화자 user_id (None = idle)
    speaker: Option<String>,

    /// 새 화자의 첫 패킷 기준값 (switch_speaker 시점에는 미정, 첫 RTP 도착 시 설정)
    origin_base_seq: u16,
    origin_base_ts: u32,

    /// 가상 시퀀스/타임스탬프 연속값
    virtual_base_seq: u16,
    virtual_base_ts: u32,

    /// 이전 화자의 마지막 가상 seq/ts (화자 전환 시 연속성 유지용)
    last_virtual_seq: u16,
    last_virtual_ts: u32,

    /// 화자 전환 후 첫 패킷 감지용
    awaiting_first_packet: bool,

    /// 비디오 전용: 키프레임 도착 대기 중 (true = P-frame 드롭)
    pending_keyframe: bool,

    /// clear_speaker 시점 기록 (idle 경과 시간 → ts_guard_gap 동적 계산)
    cleared_at: Option<Instant>,
}

/// Audio용 ts 간격: Opus 48kHz, 20ms ptime = 960 samples
pub const TS_GUARD_GAP_AUDIO: u32 = 960;
/// Video용 ts 간격: VP8 90kHz, ~33ms (1프레임분) = 3000 ticks
pub const TS_GUARD_GAP_VIDEO: u32 = 3000;

/// Opus DTX silence frame: TOC=0xf8 (48kHz, 20ms, code0) + silence data
const OPUS_SILENCE: [u8; 3] = [0xf8, 0xff, 0xfe];
/// Opus PT (서버 코덱 정책)
const OPUS_PT: u8 = 111;
/// clear_speaker 시 주입할 silence 프레임 수
const SILENCE_FLUSH_COUNT: usize = 3;

impl PttRewriter {
    /// Audio용 PttRewriter (키프레임 대기 없음)
    pub fn new_audio() -> Self {
        Self::new(TS_GUARD_GAP_AUDIO, false)
    }

    /// Video용 PttRewriter (키프레임 대기 있음)
    pub fn new_video() -> Self {
        Self::new(TS_GUARD_GAP_VIDEO, true)
    }

    fn new(ts_guard_gap: u32, require_keyframe: bool) -> Self {
        let virtual_ssrc = rand_u32();
        Self {
            virtual_ssrc,
            ts_guard_gap,
            require_keyframe,
            state: Mutex::new(RewriteState {
                speaker: None,
                origin_base_seq: 0,
                origin_base_ts: 0,
                virtual_base_seq: 0,
                virtual_base_ts: 0,
                last_virtual_seq: 0,
                last_virtual_ts: 0,
                awaiting_first_packet: false,
                pending_keyframe: false,
                cleared_at: None,
            }),
        }
    }

    /// 가상 SSRC 조회
    pub fn virtual_ssrc(&self) -> u32 {
        self.virtual_ssrc
    }

    /// 화자 전환 — Floor Granted 시점에 handler에서 호출
    pub fn switch_speaker(&self, user_id: &str) {
        let mut s = self.state.lock().unwrap();

        // Audio/Video: idle 경과 시간 → ts_guard_gap 동적 계산
        // NetEQ/jitter buffer가 arrival_time gap과 RTP ts gap의 불일치를 jitter로 해석하지 않도록
        // 실제 idle 경과 시간을 ts에 반영. Audio와 Video 모두 동일 원리 적용.
        if !self.require_keyframe {
            // Audio rewriter — dynamic ts_gap (idle 경과 시간 반영)
            let dynamic_ts_gap = if let Some(cleared) = s.cleared_at {
                let elapsed_ms = cleared.elapsed().as_millis() as u32;
                let gap = elapsed_ms.saturating_mul(48); // 48kHz: 48 samples/ms
                let gap = gap.max(self.ts_guard_gap);     // 최소 960 (20ms)
                info!("[PTT:REWRITE] audio dynamic ts_gap: idle={}ms → ts_gap={} (vs fixed={})",
                    elapsed_ms, gap, self.ts_guard_gap);
                gap
            } else {
                self.ts_guard_gap
            };

            // silence flush가 last_virtual_ts를 이미 +2880 올려놓았으므로
            // (dynamic_gap - silence분)을 추가
            let silence_ts = TS_GUARD_GAP_AUDIO * SILENCE_FLUSH_COUNT as u32;
            let extra_gap = dynamic_ts_gap.saturating_sub(silence_ts);
            s.virtual_base_ts = s.last_virtual_ts.wrapping_add(extra_gap);
            // seq는 silence flush가 이미 +3 했으므로 +1만
            s.virtual_base_seq = s.last_virtual_seq.wrapping_add(1);
        } else {
            // Video rewriter — dynamic ts_gap (idle 경과 시간 반영)
            // arrival_time gap과 RTP ts gap 불일치 → jitter buffer 오동작 방지
            // Audio와 동일 원리: 실제 idle 시간을 ts에 반영
            let dynamic_ts_gap = if let Some(cleared) = s.cleared_at {
                let elapsed_ms = cleared.elapsed().as_millis() as u32;
                let gap = elapsed_ms.saturating_mul(90); // 90kHz: 90 ticks/ms
                let gap = gap.max(self.ts_guard_gap);     // 최소 3000 (~33ms)
                info!("[PTT:REWRITE] video dynamic ts_gap: idle={}ms → ts_gap={} (vs fixed={})",
                    elapsed_ms, gap, self.ts_guard_gap);
                gap
            } else {
                self.ts_guard_gap
            };
            s.virtual_base_ts = s.last_virtual_ts.wrapping_add(dynamic_ts_gap);
            s.virtual_base_seq = s.last_virtual_seq.wrapping_add(1);
        }
        s.cleared_at = None;

        info!("[PTT:REWRITE] switch_speaker={} virtual_ssrc=0x{:08X} keyframe_wait={} v_base_seq={} v_base_ts={}",
            user_id, self.virtual_ssrc, self.require_keyframe, s.virtual_base_seq, s.virtual_base_ts);
        s.speaker = Some(user_id.to_string());
        s.awaiting_first_packet = true;
        s.pending_keyframe = self.require_keyframe;
    }

    /// 발화권 해제/회수 — Floor Release/Revoke 시점에 호출.
    /// Audio rewriter는 silence flush 프레임을 반환하고, Video는 None.
    pub fn clear_speaker(&self) -> Option<Vec<Vec<u8>>> {
        let mut s = self.state.lock().unwrap();
        s.speaker = None;
        s.awaiting_first_packet = false;
        s.pending_keyframe = false;
        s.cleared_at = Some(Instant::now());

        // Audio rewriter만 silence flush 생성 (Video는 해당 없음)
        if self.require_keyframe {
            // Video rewriter → silence 불필요
            return None;
        }

        // last_virtual_seq/ts가 0이면 아직 한 번도 리라이팅 안 함 → skip
        if s.last_virtual_seq == 0 && s.last_virtual_ts == 0 {
            return None;
        }

        let mut frames = Vec::with_capacity(SILENCE_FLUSH_COUNT);
        for i in 0..SILENCE_FLUSH_COUNT {
            let seq = s.last_virtual_seq.wrapping_add(1 + i as u16);
            let ts = s.last_virtual_ts.wrapping_add(TS_GUARD_GAP_AUDIO * (1 + i as u32));

            // RTP 헤더 (12바이트) + Opus silence payload (3바이트)
            let mut pkt = vec![0u8; 12 + OPUS_SILENCE.len()];
            // V=2, P=0, X=0, CC=0
            pkt[0] = 0x80;
            // PT = OPUS_PT, Marker=0
            pkt[1] = OPUS_PT;
            // Sequence Number
            let seq_bytes = seq.to_be_bytes();
            pkt[2] = seq_bytes[0];
            pkt[3] = seq_bytes[1];
            // Timestamp
            let ts_bytes = ts.to_be_bytes();
            pkt[4] = ts_bytes[0];
            pkt[5] = ts_bytes[1];
            pkt[6] = ts_bytes[2];
            pkt[7] = ts_bytes[3];
            // SSRC = virtual_ssrc
            let ssrc_bytes = self.virtual_ssrc.to_be_bytes();
            pkt[8]  = ssrc_bytes[0];
            pkt[9]  = ssrc_bytes[1];
            pkt[10] = ssrc_bytes[2];
            pkt[11] = ssrc_bytes[3];
            // Opus silence payload
            pkt[12..].copy_from_slice(&OPUS_SILENCE);

            frames.push(pkt);
        }

        // last_virtual 갱신 (다음 화자가 이어받을 때 연속성 유지)
        s.last_virtual_seq = s.last_virtual_seq.wrapping_add(SILENCE_FLUSH_COUNT as u16);
        s.last_virtual_ts = s.last_virtual_ts.wrapping_add(
            TS_GUARD_GAP_AUDIO * SILENCE_FLUSH_COUNT as u32
        );

        info!("[PTT:REWRITE] silence flush {} frames, last_seq={} last_ts={}",
            SILENCE_FLUSH_COUNT, s.last_virtual_seq, s.last_virtual_ts);

        Some(frames)
    }

    /// RTP plaintext를 in-place 리라이팅.
    ///
    /// - `RewriteResult::Ok`: 리라이팅 성공, fan-out 대상
    /// - `RewriteResult::PendingKeyframe`: 키프레임 대기 중, 드롭
    /// - `RewriteResult::Skip`: 리라이팅 대상 아님
    ///
    /// `is_keyframe`: 비디오 전용. 이 패킷이 VP8 I-frame인지 여부.
    ///                오디오 호출 시 false 전달.
    pub fn rewrite(&self, plaintext: &mut [u8], sender_user_id: &str, is_keyframe: bool) -> RewriteResult {
        if plaintext.len() < 12 { return RewriteResult::Skip; }

        let mut s = self.state.lock().unwrap();

        // 현재 화자가 아니면 리라이팅 안 함
        match &s.speaker {
            Some(speaker) if speaker == sender_user_id => {},
            _ => return RewriteResult::Skip,
        }

        // 비디오 키프레임 대기 중: I-frame이 올 때까지 드롭
        if s.pending_keyframe {
            if is_keyframe {
                s.pending_keyframe = false;
                debug!("[PTT:REWRITE] keyframe arrived user={} ssrc=0x{:08X}",
                    sender_user_id, self.virtual_ssrc);
                // 키프레임이 오면 awaiting_first_packet도 여기서 처리됨 (아래 분기로)
            } else {
                return RewriteResult::PendingKeyframe;
            }
        }

        // 원본 RTP 헤더 파싱
        let orig_seq = u16::from_be_bytes([plaintext[2], plaintext[3]]);
        let orig_ts  = u32::from_be_bytes([plaintext[4], plaintext[5], plaintext[6], plaintext[7]]);

        // 화자 전환 후 첫 패킷: origin base 설정
        let is_first = s.awaiting_first_packet;
        if is_first {
            s.origin_base_seq = orig_seq;
            s.origin_base_ts = orig_ts;

            // Audio/Video 모두 switch_speaker에서 dynamic ts_gap 기반 v_base 설정 완료
            // 여기서는 origin_base만 설정
            s.awaiting_first_packet = false;

            debug!("[PTT:REWRITE] first_pkt user={} orig_seq={} orig_ts={} → v_base_seq={} v_base_ts={}",
                sender_user_id, orig_seq, orig_ts, s.virtual_base_seq, s.virtual_base_ts);
        }

        // 오프셋 기반 상대 연산
        let virtual_seq = orig_seq.wrapping_sub(s.origin_base_seq)
                                  .wrapping_add(s.virtual_base_seq);
        let virtual_ts  = orig_ts.wrapping_sub(s.origin_base_ts)
                                 .wrapping_add(s.virtual_base_ts);

        // 마지막 가상값 기록 (다음 화자 전환 시 연속성 유지)
        s.last_virtual_seq = virtual_seq;
        s.last_virtual_ts = virtual_ts;

        // RTP 헤더 in-place 덮어쓰기
        // SSRC: bytes 8..12
        let ssrc_bytes = self.virtual_ssrc.to_be_bytes();
        plaintext[8]  = ssrc_bytes[0];
        plaintext[9]  = ssrc_bytes[1];
        plaintext[10] = ssrc_bytes[2];
        plaintext[11] = ssrc_bytes[3];

        // Sequence Number: bytes 2..4
        let seq_bytes = virtual_seq.to_be_bytes();
        plaintext[2] = seq_bytes[0];
        plaintext[3] = seq_bytes[1];

        // Timestamp: bytes 4..8
        let ts_bytes = virtual_ts.to_be_bytes();
        plaintext[4] = ts_bytes[0];
        plaintext[5] = ts_bytes[1];
        plaintext[6] = ts_bytes[2];
        plaintext[7] = ts_bytes[3];

        // 화자 전환 첫 패킷: marker bit 설정 (Audio 전용)
        // Opus는 1패킷=1프레임이라 marker 강제가 무해.
        // VP8는 키프레임이 다중 패킷으로 분할되므로, 첫 패킷에 marker=1을 설정하면
        // Chrome이 "이 패킷이 전체 프레임"으로 오인 → 불완전 디코딩 → freeze.
        if is_first && !self.require_keyframe {
            plaintext[1] |= 0x80;
        }

        RewriteResult::Ok
    }

    /// SR의 RTP timestamp를 가상 공간으로 변환
    /// publisher SR의 NTP↔RTP 관계를 subscriber 가상 스트림 기준으로 보정.
    /// 화자가 없거나 첫 패킷 미도착 시 None 반환.
    pub fn translate_rtp_ts(&self, original_rtp_ts: u32) -> Option<u32> {
        let s = self.state.lock().unwrap();
        if s.speaker.is_none() || s.awaiting_first_packet {
            return None;
        }
        Some(
            original_rtp_ts
                .wrapping_sub(s.origin_base_ts)
                .wrapping_add(s.virtual_base_ts)
        )
    }

    /// NACK 역매핑: 가상 seq → 원본 seq 역산
    pub fn reverse_seq(&self, virtual_seq: u16) -> u16 {
        let s = self.state.lock().unwrap();
        virtual_seq.wrapping_sub(s.virtual_base_seq)
                   .wrapping_add(s.origin_base_seq)
    }

    /// 현재 화자인지 확인
    pub fn is_current_speaker(&self, user_id: &str) -> bool {
        let s = self.state.lock().unwrap();
        match &s.speaker {
            Some(speaker) => speaker == user_id,
            None => false,
        }
    }
}

/// VP8 키프레임 감지 (RFC 7741)
/// RTP payload 첫 바이트의 VP8 payload descriptor에서 I-frame 여부 판별
/// VP8 payload descriptor: 1st octet bit0(S) + bit4(P)
///   P=0 → key frame, P=1 → inter frame (delta)
pub fn is_vp8_keyframe(rtp_plaintext: &[u8]) -> bool {
    if rtp_plaintext.len() < 13 { return false; }

    // RTP 헤더 크기 계산 (고정 12 + CSRC + extension)
    let cc = (rtp_plaintext[0] & 0x0F) as usize;
    let has_ext = (rtp_plaintext[0] & 0x10) != 0;
    let mut offset = 12 + cc * 4;

    if has_ext {
        if rtp_plaintext.len() < offset + 4 { return false; }
        let ext_len = u16::from_be_bytes([rtp_plaintext[offset + 2], rtp_plaintext[offset + 3]]) as usize;
        offset += 4 + ext_len * 4;
    }

    if rtp_plaintext.len() <= offset { return false; }

    // VP8 payload descriptor: 첫 바이트
    let pd = rtp_plaintext[offset];

    // X bit (extension present): bit 7
    let x = (pd & 0x80) != 0;
    // S bit (start of VP8 partition): bit 4
    let s = (pd & 0x10) != 0;

    if !s { return false; } // 파티션 시작이 아니면 키프레임 판별 불가

    // VP8 payload descriptor 확장 바이트 건너뛰기
    let mut pd_offset = offset + 1;
    if x {
        if rtp_plaintext.len() <= pd_offset { return false; }
        let x_byte = rtp_plaintext[pd_offset];
        pd_offset += 1;
        // I (PictureID present)
        if (x_byte & 0x80) != 0 {
            if rtp_plaintext.len() <= pd_offset { return false; }
            if (rtp_plaintext[pd_offset] & 0x80) != 0 {
                pd_offset += 2; // 16-bit PictureID
            } else {
                pd_offset += 1; // 8-bit PictureID
            }
        }
        // L (TL0PICIDX present)
        if (x_byte & 0x40) != 0 { pd_offset += 1; }
        // T|K (TID/KEYIDX present)
        if (x_byte & 0x20) != 0 || (x_byte & 0x10) != 0 { pd_offset += 1; }
    }

    // VP8 bitstream: 첫 바이트의 bit0 = frame type (0=key, 1=inter)
    if rtp_plaintext.len() <= pd_offset { return false; }
    let frame_byte = rtp_plaintext[pd_offset];
    (frame_byte & 0x01) == 0
}

/// VP8 진단 정보 (임시 디버그 — 키프레임 미도착 원인 추적용)
pub struct Vp8Diag {
    pub is_keyframe: bool,
    pub pd_bytes: [u8; 4],
    pub frame_byte: Option<u8>,
    pub s_bit: bool,
    pub payload_offset: usize,
    pub payload_len: usize,
}

/// VP8 payload descriptor 진단 — is_vp8_keyframe()과 동일 파싱 경로로 상세 정보 반환
pub fn diagnose_vp8(rtp_plaintext: &[u8]) -> Option<Vp8Diag> {
    if rtp_plaintext.len() < 13 { return None; }

    let cc = (rtp_plaintext[0] & 0x0F) as usize;
    let has_ext = (rtp_plaintext[0] & 0x10) != 0;
    let mut offset = 12 + cc * 4;

    if has_ext {
        if rtp_plaintext.len() < offset + 4 { return None; }
        let ext_len = u16::from_be_bytes([
            rtp_plaintext[offset + 2], rtp_plaintext[offset + 3],
        ]) as usize;
        offset += 4 + ext_len * 4;
    }

    if rtp_plaintext.len() <= offset { return None; }

    let payload_offset = offset;
    let payload_len = rtp_plaintext.len() - offset;
    let pd = rtp_plaintext[offset];
    let x = (pd & 0x80) != 0;
    let s = (pd & 0x10) != 0;

    // PD 첫 4바이트 수집
    let mut pd_bytes = [0u8; 4];
    let avail = payload_len.min(4);
    pd_bytes[..avail].copy_from_slice(&rtp_plaintext[payload_offset..payload_offset + avail]);

    // frame byte 추출 (is_vp8_keyframe 동일 경로)
    let mut pd_offset = offset + 1;
    if x {
        if rtp_plaintext.len() <= pd_offset {
            return Some(Vp8Diag { is_keyframe: false, pd_bytes, frame_byte: None, s_bit: s, payload_offset, payload_len });
        }
        let x_byte = rtp_plaintext[pd_offset];
        pd_offset += 1;
        if (x_byte & 0x80) != 0 {
            if rtp_plaintext.len() <= pd_offset {
                return Some(Vp8Diag { is_keyframe: false, pd_bytes, frame_byte: None, s_bit: s, payload_offset, payload_len });
            }
            if (rtp_plaintext[pd_offset] & 0x80) != 0 { pd_offset += 2; } else { pd_offset += 1; }
        }
        if (x_byte & 0x40) != 0 { pd_offset += 1; }
        if (x_byte & 0x20) != 0 || (x_byte & 0x10) != 0 { pd_offset += 1; }
    }

    let frame_byte = if rtp_plaintext.len() > pd_offset {
        Some(rtp_plaintext[pd_offset])
    } else {
        None
    };

    let is_keyframe = s && frame_byte.map(|b| (b & 0x01) == 0).unwrap_or(false);

    Some(Vp8Diag { is_keyframe, pd_bytes, frame_byte, s_bit: s, payload_offset, payload_len })
}

/// 랜덤 u32 생성
fn rand_u32() -> u32 {
    let mut buf = [0u8; 4];
    getrandom::fill(&mut buf).expect("getrandom failed");
    u32::from_be_bytes(buf)
}
