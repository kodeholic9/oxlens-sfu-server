// author: kodeholic (powered by Claude)
//! RTCP Terminator — 서버 자체 RR/SR 생성
//!
//! SFU는 두 개의 독립된 RTP 세션의 종단점(peer)이다:
//!   - Ingress: Publisher → SFU (서버가 수신자 → RR 생성)
//!   - Egress:  SFU → Subscriber (서버가 송신자 → SR 생성)
//!
//! 수신자 RR은 publisher에게 릴레이하지 않고 서버가 소비한다.
//! publisher의 SR도 subscriber에게 릴레이하지 않고 서버가 소비한다.
//!
//! RFC 3550 Appendix A.3 (loss 계산), A.8 (jitter 추정)
//! RFC 8079 (SFU RTCP 종단 가이드라인)

use std::time::Instant;
use crate::config;

// ============================================================================
// RecvStats — per-publisher 수신 통계 (RFC 3550 A.3 + A.8)
// ============================================================================

/// Publisher SSRC별 수신 통계. Participant.recv_stats에 per-track으로 소속.
/// ingress hot path에서 on_rtp_received()로 갱신, 타이머에서 build_rr_block()으로 소비.
pub struct RecvStats {
    pub ssrc: u32,
    pub clock_rate: u32,

    // --- seq tracking (RFC 3550 A.1) ---
    max_seq: u16,
    cycles: u32,         // seq wraparound count × 65536
    base_seq: u32,       // 최초 수신 seq (extended)
    bad_seq: u32,        // 순서 꼬임 감지용
    received: u32,       // 실제 수신 패킷 수

    // --- loss calculation snapshot (RFC 3550 A.3) ---
    expected_prior: u32,
    received_prior: u32,

    // --- jitter (RFC 3550 A.8) ---
    jitter: f64,
    last_transit: i64,

    // --- SR 연동 (RTT 계산용) ---
    last_sr_ntp_middle: u32,  // 마지막 수신 SR의 NTP 중간 32비트
    last_sr_received_at: Option<Instant>,

    // --- 초기화 ---
    initialized: bool,
    seq_probation: u32,       // min_sequential 카운터
}

impl RecvStats {
    pub fn new(ssrc: u32, clock_rate: u32) -> Self {
        Self {
            ssrc,
            clock_rate,
            max_seq: 0,
            cycles: 0,
            base_seq: 0,
            bad_seq: 0,
            received: 0,
            expected_prior: 0,
            received_prior: 0,
            jitter: 0.0,
            last_transit: 0,
            last_sr_ntp_middle: 0,
            last_sr_received_at: None,
            initialized: false,
            seq_probation: config::RTP_SEQ_MIN_SEQUENTIAL,
        }
    }

    /// 첫 패킷으로 시퀀스 초기화 (RFC 3550 A.1 init_seq)
    fn init_seq(&mut self, seq: u16) {
        self.base_seq = seq as u32;
        self.max_seq = seq;
        self.bad_seq = (seq as u32).wrapping_add(1); // RTP_SEQ_MOD
        self.cycles = 0;
        self.received = 0;
        self.received_prior = 0;
        self.expected_prior = 0;
    }

    /// RTP 수신 시 호출 — seq/jitter 갱신 (ingress hot path)
    ///
    /// `arrival_ms`: 패킷 도착 시각 (밀리초, monotonic)
    /// `rtp_ts`: RTP timestamp
    pub fn update(&mut self, seq: u16, rtp_ts: u32, arrival_ms: u64) {
        if !self.initialized {
            self.init_seq(seq);
            self.initialized = true;
            self.seq_probation = config::RTP_SEQ_MIN_SEQUENTIAL - 1;
            self.received += 1;
            self.update_jitter(rtp_ts, arrival_ms);
            return;
        }

        // probation 구간: 연속 seq 확인
        if self.seq_probation > 0 {
            if seq == self.max_seq.wrapping_add(1) {
                self.seq_probation -= 1;
                self.max_seq = seq;
                if self.seq_probation == 0 {
                    self.init_seq(seq);
                    self.received += 1;
                    self.update_jitter(rtp_ts, arrival_ms);
                    return;
                }
            } else {
                self.seq_probation = config::RTP_SEQ_MIN_SEQUENTIAL - 1;
                self.max_seq = seq;
            }
            self.received += 1;
            return;
        }

        let udelta = seq.wrapping_sub(self.max_seq);

        if udelta < config::RTP_SEQ_MAX_DROPOUT {
            // 정상 순서 또는 약간의 reorder
            if seq < self.max_seq {
                // seq wrapped around
                self.cycles += 65536;
            }
            self.max_seq = seq;
        } else if udelta <= 65535u16.wrapping_sub(config::RTP_SEQ_MAX_MISORDER) {
            // 큰 점프 — 새 소스이거나 심한 reorder
            if seq as u32 == self.bad_seq {
                // 두 번 연속 같은 "bad" seq → 소스 재시작으로 판단
                self.init_seq(seq);
            } else {
                self.bad_seq = (seq as u32).wrapping_add(1); // & (RTP_SEQ_MOD - 1)
                return; // 이 패킷은 무시
            }
        } else {
            // 약간의 late arrival (reorder) — 카운트만 함
        }

        self.received += 1;
        self.update_jitter(rtp_ts, arrival_ms);
    }

    /// Jitter 갱신 (RFC 3550 A.8)
    fn update_jitter(&mut self, rtp_ts: u32, arrival_ms: u64) {
        // arrival을 RTP clock 단위로 변환
        let arrival_rtp = (arrival_ms as i64) * (self.clock_rate as i64) / 1000;
        let transit = arrival_rtp - (rtp_ts as i64);

        if self.last_transit != 0 {
            let d = (transit - self.last_transit).abs() as f64;
            // J(i) = J(i-1) + (|D(i-1,i)| - J(i-1)) / 16
            self.jitter += (d - self.jitter) / 16.0;
        }
        self.last_transit = transit;
    }

    /// Publisher SR 수신 시 호출 — LSR/DLSR 계산용 저장
    pub fn on_sr_received(&mut self, ntp_ts_hi: u32, ntp_ts_lo: u32) {
        // NTP 중간 32비트: hi의 하위 16비트 + lo의 상위 16비트
        self.last_sr_ntp_middle = ((ntp_ts_hi & 0xFFFF) << 16) | (ntp_ts_lo >> 16);
        self.last_sr_received_at = Some(Instant::now());
    }

    /// RR Report Block 생성 (RFC 3550 Section 6.4.1)
    /// 호출 후 expected_prior/received_prior 스냅샷 갱신
    pub fn build_rr_block(&mut self) -> RrReportBlock {
        let extended_max = self.cycles as u32 + self.max_seq as u32;
        let expected = extended_max.wrapping_sub(self.base_seq).wrapping_add(1);
        let lost = if expected > self.received {
            expected - self.received
        } else {
            0
        };

        // fraction lost (이전 RR 이후 구간)
        let expected_interval = expected.wrapping_sub(self.expected_prior);
        let received_interval = self.received.wrapping_sub(self.received_prior);
        let lost_interval = if expected_interval > received_interval {
            expected_interval - received_interval
        } else {
            0
        };
        let fraction = if expected_interval > 0 {
            ((lost_interval as u64 * 256) / expected_interval as u64) as u8
        } else {
            0
        };

        // 스냅샷 갱신
        self.expected_prior = expected;
        self.received_prior = self.received;

        // cumulative lost: 24비트 signed, 클램핑
        let cum_lost = lost.min(0x7FFFFF);

        // DLSR: last_sr_received_at 이후 경과 (1/65536초 단위)
        let dlsr = self.last_sr_received_at
            .map(|t| {
                let elapsed = t.elapsed();
                let secs = elapsed.as_secs() as u32;
                let frac = (elapsed.subsec_nanos() as u64 * 65536 / 1_000_000_000) as u32;
                secs.wrapping_mul(65536).wrapping_add(frac)
            })
            .unwrap_or(0);

        RrReportBlock {
            ssrc: self.ssrc,
            fraction_lost: fraction,
            cumulative_lost: cum_lost,
            extended_highest_seq: extended_max,
            jitter: self.jitter as u32,
            last_sr: self.last_sr_ntp_middle,
            delay_since_last_sr: dlsr,
        }
    }
}

// ============================================================================
// RR Report Block
// ============================================================================

/// RFC 3550 RR Report Block (24 bytes)
pub struct RrReportBlock {
    pub ssrc: u32,
    pub fraction_lost: u8,
    pub cumulative_lost: u32,   // 24비트 사용
    pub extended_highest_seq: u32,
    pub jitter: u32,
    pub last_sr: u32,
    pub delay_since_last_sr: u32,
}

// ============================================================================
// RR 패킷 빌더
// ============================================================================

/// RTCP Receiver Report 패킷 생성 (RFC 3550 Section 6.4.2)
///
/// 서버가 publisher에게 보내는 RR. sender_ssrc는 서버 자체 SSRC (임의값).
pub fn build_receiver_report(sender_ssrc: u32, blocks: &[RrReportBlock]) -> Vec<u8> {
    let rc = blocks.len().min(31) as u8;
    // header(4) + sender_ssrc(4) + report_block(24) × rc
    let total_len = 8 + 24 * rc as usize;
    let length_field = (total_len / 4) - 1;

    let mut buf = vec![0u8; total_len];

    // V=2, P=0, RC
    buf[0] = 0x80 | rc;
    // PT = 201 (RR)
    buf[1] = config::RTCP_PT_RR;
    // length
    buf[2..4].copy_from_slice(&(length_field as u16).to_be_bytes());
    // sender SSRC
    buf[4..8].copy_from_slice(&sender_ssrc.to_be_bytes());

    for (i, block) in blocks.iter().enumerate().take(rc as usize) {
        let off = 8 + i * 24;
        // SSRC_n
        buf[off..off + 4].copy_from_slice(&block.ssrc.to_be_bytes());
        // fraction lost (8) + cumulative lost (24)
        buf[off + 4] = block.fraction_lost;
        let cum = block.cumulative_lost & 0x00FFFFFF;
        buf[off + 5] = ((cum >> 16) & 0xFF) as u8;
        buf[off + 6] = ((cum >> 8) & 0xFF) as u8;
        buf[off + 7] = (cum & 0xFF) as u8;
        // extended highest seq
        buf[off + 8..off + 12].copy_from_slice(&block.extended_highest_seq.to_be_bytes());
        // jitter
        buf[off + 12..off + 16].copy_from_slice(&block.jitter.to_be_bytes());
        // LSR
        buf[off + 16..off + 20].copy_from_slice(&block.last_sr.to_be_bytes());
        // DLSR
        buf[off + 20..off + 24].copy_from_slice(&block.delay_since_last_sr.to_be_bytes());
    }

    buf
}

// ============================================================================
// SR 빌더 (Egress: 서버 → Subscriber)
// ============================================================================

/// SendStats — subscriber 방향 송신 통계 (SR 생성용)
/// egress task에서 갱신, 타이머에서 SR 생성 시 참조.
pub struct SendStats {
    pub ssrc: u32,
    pub packets_sent: u32,
    pub bytes_sent: u32,
    pub last_rtp_ts: u32,
    pub last_send_time: Option<Instant>,
    pub clock_rate: u32,
}

impl SendStats {
    pub fn new(ssrc: u32, clock_rate: u32) -> Self {
        Self {
            ssrc,
            packets_sent: 0,
            bytes_sent: 0,
            last_rtp_ts: 0,
            last_send_time: None,
            clock_rate,
        }
    }

    /// RTP 송신 시 호출
    pub fn on_rtp_sent(&mut self, rtp_ts: u32, payload_len: usize) {
        self.packets_sent += 1;
        self.bytes_sent += payload_len as u32;
        self.last_rtp_ts = rtp_ts;
        self.last_send_time = Some(Instant::now());
    }
}

/// RTCP Sender Report 생성 (RFC 3550 Section 6.4.1)
///
/// 서버가 subscriber에게 보내는 SR. 서버가 보낸 패킷 수/바이트/timestamp 포함.
pub fn build_sender_report(stats: &SendStats) -> Vec<u8> {
    // header(4) + sender_ssrc(4) + sender_info(20) = 28 bytes (no report blocks)
    let total_len = 28;
    let length_field = (total_len / 4) - 1;

    let mut buf = vec![0u8; total_len];

    // V=2, P=0, RC=0
    buf[0] = 0x80;
    // PT = 200 (SR)
    buf[1] = config::RTCP_PT_SR;
    // length
    buf[2..4].copy_from_slice(&(length_field as u16).to_be_bytes());
    // sender SSRC
    buf[4..8].copy_from_slice(&stats.ssrc.to_be_bytes());

    // NTP timestamp (현재 wallclock → NTP 64비트)
    let (ntp_hi, ntp_lo) = wallclock_to_ntp();
    buf[8..12].copy_from_slice(&ntp_hi.to_be_bytes());
    buf[12..16].copy_from_slice(&ntp_lo.to_be_bytes());

    // RTP timestamp: 마지막 송신 ts + 경과분 보정
    let rtp_ts = if let Some(t) = stats.last_send_time {
        let elapsed_ms = t.elapsed().as_millis() as u32;
        let elapsed_ticks = elapsed_ms * stats.clock_rate / 1000;
        stats.last_rtp_ts.wrapping_add(elapsed_ticks)
    } else {
        0
    };
    buf[16..20].copy_from_slice(&rtp_ts.to_be_bytes());

    // sender's packet count
    buf[20..24].copy_from_slice(&stats.packets_sent.to_be_bytes());
    // sender's octet count
    buf[24..28].copy_from_slice(&stats.bytes_sent.to_be_bytes());

    buf
}

/// 현재 wallclock → NTP 64비트 timestamp
/// NTP epoch = 1900-01-01, Unix epoch = 1970-01-01 (차이 70년 = 2208988800초)
fn wallclock_to_ntp() -> (u32, u32) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let ntp_secs = now.as_secs() + 2_208_988_800;
    let ntp_frac = ((now.subsec_nanos() as u64) << 32) / 1_000_000_000;
    (ntp_secs as u32, ntp_frac as u32)
}

// ============================================================================
// SR 파싱 (publisher SR에서 NTP timestamp 추출)
// ============================================================================

/// Publisher SR에서 NTP timestamp 추출 (RecvStats.on_sr_received 호출용)
/// SR 패킷 최소 크기: header(4) + ssrc(4) + sender_info(20) = 28
pub fn parse_sr_ntp(sr_block: &[u8]) -> Option<(u32, u32, u32)> {
    if sr_block.len() < 28 { return None; }
    let pt = sr_block[1];
    if pt != config::RTCP_PT_SR { return None; }
    let ssrc = u32::from_be_bytes([sr_block[4], sr_block[5], sr_block[6], sr_block[7]]);
    let ntp_hi = u32::from_be_bytes([sr_block[8], sr_block[9], sr_block[10], sr_block[11]]);
    let ntp_lo = u32::from_be_bytes([sr_block[12], sr_block[13], sr_block[14], sr_block[15]]);
    Some((ssrc, ntp_hi, ntp_lo))
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_recv_stats_basic_loss() {
        let mut stats = RecvStats::new(0x12345678, 48000);

        // seq 0, 1, 2, 3 수신 (loss 없음)
        for i in 0..4u16 {
            stats.update(i, i as u32 * 960, i as u64 * 20);
        }

        let block = stats.build_rr_block();
        assert_eq!(block.fraction_lost, 0);
        assert_eq!(block.cumulative_lost, 0);
    }

    #[test]
    fn test_recv_stats_with_loss() {
        let mut stats = RecvStats::new(0x12345678, 48000);

        // seq 0, 1, 2 수신 → 3,4 누락 → 5 수신
        for i in 0..3u16 {
            stats.update(i, i as u32 * 960, i as u64 * 20);
        }
        // 3, 4 누락
        stats.update(5, 5 * 960, 5 * 20);

        let block = stats.build_rr_block();
        // expected=6 (0~5), received=4 → lost=2
        assert_eq!(block.cumulative_lost, 2);
        assert!(block.fraction_lost > 0);
    }

    #[test]
    fn test_recv_stats_jitter() {
        let mut stats = RecvStats::new(0x12345678, 48000);

        // 균일한 간격으로 수신 → jitter ≈ 0
        for i in 0..10u16 {
            stats.update(i, i as u32 * 960, i as u64 * 20);
        }
        let block = stats.build_rr_block();
        // 균일 도착이면 jitter가 매우 작아야 함
        assert!(block.jitter < 100);
    }

    #[test]
    fn test_build_receiver_report_format() {
        let block = RrReportBlock {
            ssrc: 0xAABBCCDD,
            fraction_lost: 25,
            cumulative_lost: 100,
            extended_highest_seq: 5000,
            jitter: 50,
            last_sr: 0x11223344,
            delay_since_last_sr: 0x55667788,
        };
        let rr = build_receiver_report(0x00000001, &[block]);
        assert_eq!(rr.len(), 32); // 8 + 24
        assert_eq!(rr[0] & 0xC0, 0x80); // V=2
        assert_eq!(rr[0] & 0x1F, 1);    // RC=1
        assert_eq!(rr[1], config::RTCP_PT_RR);
    }

    #[test]
    fn test_build_sender_report_format() {
        let stats = SendStats {
            ssrc: 0xDEADBEEF,
            packets_sent: 1000,
            bytes_sent: 160_000,
            last_rtp_ts: 48000,
            last_send_time: Some(Instant::now()),
            clock_rate: 48000,
        };
        let sr = build_sender_report(&stats);
        assert_eq!(sr.len(), 28);
        assert_eq!(sr[0] & 0xC0, 0x80); // V=2
        assert_eq!(sr[1], config::RTCP_PT_SR);
    }

    #[test]
    fn test_sr_ntp_roundtrip() {
        let stats = SendStats {
            ssrc: 0x11111111,
            packets_sent: 1,
            bytes_sent: 100,
            last_rtp_ts: 960,
            last_send_time: Some(Instant::now()),
            clock_rate: 48000,
        };
        let sr = build_sender_report(&stats);
        let (ssrc, ntp_hi, ntp_lo) = parse_sr_ntp(&sr).unwrap();
        assert_eq!(ssrc, 0x11111111);
        assert!(ntp_hi > 0); // NTP epoch 이후이므로 0보다 커야 함
        let _ = ntp_lo; // ntp_lo는 fractional
    }
}
