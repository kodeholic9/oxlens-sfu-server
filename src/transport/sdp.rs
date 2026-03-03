// author: kodeholic (powered by Claude)
//! SDP Offer parsing + Answer generation (Phase 3)
//!
//! 설계 원칙:
//!   - 외부 크레이트 없이 직접 파싱/조립 (mini-livechat 검증 패턴)
//!   - Phase 3 범위: 초기 Offer → Answer만 (re-nego는 추후 Phase)
//!   - direction: offer의 sendrecv → answer도 sendrecv (양방향 열어둠)
//!     → 릴레이 제어는 미디어 계층(Phase 4)에서 처리
//!   - SSRC/msid: offer 것은 skip, 서버 것도 아직 안 넣음 (Phase 4)
//!   - ICE credentials: 외부 주입 (Participant.ufrag/pwd와 일치 보장)

use std::net::UdpSocket;
use tracing::warn;

// ============================================================================
// Parsed SDP types
// ============================================================================

/// Offer 파싱 결과
#[derive(Debug)]
pub struct ParsedOffer {
    pub sections: Vec<MediaSection>,
    pub bundle_mids: Vec<String>,
}

/// 개별 m= 섹션
#[derive(Debug, Clone)]
pub struct MediaSection {
    /// a=mid 값 (BUNDLE 식별자)
    pub mid: String,
    /// "audio" | "video"
    pub media_type: String,
    /// 원본 m= 라인 (포트 교체 전)
    pub m_line: String,
    /// ICE/DTLS/direction/SSRC/msid 제외 나머지 속성 라인
    pub codec_lines: Vec<String>,
    /// offer의 direction (sendrecv | sendonly | recvonly | inactive)
    pub direction: String,
    /// a=rtcp-rsize 존재 여부 (answer 미러링용)
    pub has_rtcp_rsize: bool,
    /// a=ssrc:NNNN 에서 추출한 SSRC 목록 (Track 등록용, Phase 4)
    pub ssrcs: Vec<u32>,
}

/// SDP 파싱 에러
#[derive(Debug)]
pub enum SdpError {
    /// m= 섹션이 하나도 없음
    NoMediaSection,
    /// 기타 파싱 오류
    ParseError(String),
}

impl std::fmt::Display for SdpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SdpError::NoMediaSection => write!(f, "no media section in SDP offer"),
            SdpError::ParseError(msg) => write!(f, "SDP parse error: {}", msg),
        }
    }
}

// ============================================================================
// Offer parsing
// ============================================================================

/// ICE/DTLS/direction/SSRC/msid — 서버 값으로 교체할 라인들 (파싱 시 skip)
const SKIP_PREFIXES: &[&str] = &[
    "a=ice-",
    "a=fingerprint",
    "a=setup",
    "a=candidate",
    "a=sendrecv",
    "a=sendonly",
    "a=recvonly",
    "a=inactive",
    "a=rtcp-mux",
    "a=rtcp-rsize",
    "c=",
    "a=ssrc",
    "a=ssrc-group",
    "a=msid",
];

/// SDP Offer 문자열을 파싱하여 미디어 섹션별로 분리
///
/// 파싱 알고리즘 (GUIDELINES §3 준수):
///   - m= 만나면 새 MediaSection 시작
///   - SKIP_PREFIXES 라인은 제외 (서버 값으로 교체할 것들)
///   - 단, direction/rtcp-rsize/ssrc는 값만 캡처하고 라인은 버림
///   - rtpmap, fmtp, rtcp-fb, extmap, mid 등은 CAPTURE
pub fn parse_offer(offer: &str) -> Result<ParsedOffer, SdpError> {
    let mut sections: Vec<MediaSection> = Vec::new();
    let mut current: Option<MediaSection> = None;

    for line in offer.lines() {
        let line = line.trim_end();

        if line.starts_with("m=") {
            // 이전 섹션 저장
            if let Some(sec) = current.take() {
                sections.push(sec);
            }

            let media_type = if line.starts_with("m=audio") {
                "audio"
            } else if line.starts_with("m=video") {
                "video"
            } else {
                "unknown"
            };

            current = Some(MediaSection {
                mid: String::new(),
                media_type: media_type.to_string(),
                m_line: line.to_string(),
                codec_lines: Vec::new(),
                direction: "sendrecv".to_string(),
                has_rtcp_rsize: false,
                ssrcs: Vec::new(),
            });
            continue;
        }

        let sec = match current.as_mut() {
            Some(s) => s,
            None => continue, // 세션 헤더 영역 — skip
        };

        // direction 캡처 (라인 자체는 skip)
        if line.starts_with("a=sendrecv") {
            sec.direction = "sendrecv".to_string();
            continue;
        }
        if line.starts_with("a=recvonly") {
            sec.direction = "recvonly".to_string();
            continue;
        }
        if line.starts_with("a=sendonly") {
            sec.direction = "sendonly".to_string();
            continue;
        }
        if line.starts_with("a=inactive") {
            sec.direction = "inactive".to_string();
            continue;
        }

        // rtcp-rsize 캡처
        if line.starts_with("a=rtcp-rsize") {
            sec.has_rtcp_rsize = true;
            // skip — answer에서 별도 출력
        }

        // SSRC 캡처 (Phase 4 Track 등록용)
        if line.starts_with("a=ssrc:") {
            if let Some(ssrc_str) = line.strip_prefix("a=ssrc:")
                .and_then(|s| s.split_whitespace().next())
                .and_then(|s| s.split(':').next())
            {
                if let Ok(ssrc) = ssrc_str.parse::<u32>() {
                    if !sec.ssrcs.contains(&ssrc) {
                        sec.ssrcs.push(ssrc);
                    }
                }
            }
            // skip — answer에 echo하지 않음
        }

        // skip 대상 라인 필터
        if SKIP_PREFIXES.iter().any(|p| line.starts_with(p)) {
            continue;
        }

        // mid 캡처
        if line.starts_with("a=mid:") {
            sec.mid = line["a=mid:".len()..].trim().to_string();
        }

        // 나머지: codec/extmap/rtcp 등 보존
        sec.codec_lines.push(line.to_string());
    }

    // 마지막 섹션 저장
    if let Some(sec) = current.take() {
        sections.push(sec);
    }

    if sections.is_empty() {
        return Err(SdpError::NoMediaSection);
    }

    let bundle_mids: Vec<String> = sections.iter().map(|s| s.mid.clone()).collect();

    Ok(ParsedOffer {
        sections,
        bundle_mids,
    })
}

// ============================================================================
// Answer generation
// ============================================================================

/// SDP Answer 생성
///
/// 설계 결정:
///   - direction: offer 그대로 echo (sendrecv → sendrecv)
///   - 코덱/extmap: offer 그대로 미러링
///   - SSRC/msid: 없음 (Phase 4에서 추가)
///   - ICE: 외부 주입 (Participant 것과 일치 보장)
///   - DTLS: setup=passive (ICE-Lite 서버는 항상 passive)
///   - candidate: host 1개 (단일 UDP 포트)
pub fn build_answer(
    parsed: &ParsedOffer,
    fingerprint: &str,
    ice_ufrag: &str,
    ice_pwd: &str,
    udp_port: u16,
) -> String {
    let local_ip = detect_local_ip();
    let session_id = current_timestamp();

    let mut sdp = String::with_capacity(2048);

    // ── 세션 헤더 ──
    sdp.push_str("v=0\r\n");
    sdp.push_str(&format!(
        "o=light-livechat {} {} IN IP4 {}\r\n",
        session_id, session_id, local_ip
    ));
    sdp.push_str("s=-\r\n");
    sdp.push_str("t=0 0\r\n");
    sdp.push_str(&format!(
        "a=group:BUNDLE {}\r\n",
        parsed.bundle_mids.join(" ")
    ));
    sdp.push_str("a=ice-lite\r\n");

    // ── 미디어 섹션: offer 순서대로 미러링 ──
    for sec in &parsed.sections {
        // m= 라인: 포트만 서버 포트로 교체
        let m_line = replace_m_line_port(&sec.m_line, udp_port);
        sdp.push_str(&m_line);
        sdp.push_str("\r\n");

        // connection
        sdp.push_str(&format!("c=IN IP4 {}\r\n", local_ip));

        // ICE
        sdp.push_str(&format!("a=ice-ufrag:{}\r\n", ice_ufrag));
        sdp.push_str(&format!("a=ice-pwd:{}\r\n", ice_pwd));

        // DTLS
        sdp.push_str(&format!("a=fingerprint:{}\r\n", fingerprint));
        sdp.push_str("a=setup:passive\r\n");

        // rtcp-mux (BUNDLE 필수)
        sdp.push_str("a=rtcp-mux\r\n");

        // rtcp-rsize: offer에 있을 때만 미러링
        if sec.has_rtcp_rsize {
            sdp.push_str("a=rtcp-rsize\r\n");
        }

        // direction: offer 그대로 echo
        sdp.push_str(&format!("a={}\r\n", sec.direction));

        // 코덱/extmap/mid 등 보존된 라인들
        for line in &sec.codec_lines {
            sdp.push_str(line);
            sdp.push_str("\r\n");
        }

        // ICE candidate: host 1개
        sdp.push_str(&format!(
            "a=candidate:1 1 udp 2113937151 {} {} typ host generation 0\r\n",
            local_ip, udp_port
        ));
        sdp.push_str("a=end-of-candidates\r\n");
    }

    sdp
}

// ============================================================================
// Utility
// ============================================================================

/// m= 라인 포트 교체: "m=audio 9 ..." → "m=audio {port} ..."
fn replace_m_line_port(m_line: &str, port: u16) -> String {
    let parts: Vec<&str> = m_line.splitn(4, ' ').collect();
    if parts.len() == 4 {
        format!("{} {} {} {}", parts[0], port, parts[2], parts[3])
    } else {
        m_line.to_string()
    }
}

/// 라우팅 테이블 기반 로컬 IP 감지
/// UDP 소켓으로 8.8.8.8:80 connect (실제 패킷 없음) → local_addr() 조회
pub fn detect_local_ip() -> String {
    UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| {
            s.connect("8.8.8.8:80")?;
            s.local_addr()
        })
        .map(|addr| addr.ip().to_string())
        .unwrap_or_else(|_| {
            warn!("local IP detection failed — fallback to 127.0.0.1");
            "127.0.0.1".to_string()
        })
}

/// Unix epoch millis
fn current_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ── 테스트용 SDP offer ──

    fn make_audio_offer(ufrag: &str) -> String {
        format!(
            "v=0\r\n\
             o=- 123 2 IN IP4 0.0.0.0\r\n\
             s=-\r\n\
             t=0 0\r\n\
             a=group:BUNDLE 0\r\n\
             m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
             c=IN IP4 0.0.0.0\r\n\
             a=mid:0\r\n\
             a=ice-ufrag:{}\r\n\
             a=ice-pwd:clientpwd\r\n\
             a=fingerprint:sha-256 AA:BB\r\n\
             a=setup:actpass\r\n\
             a=sendrecv\r\n\
             a=rtcp-mux\r\n\
             a=rtpmap:111 opus/48000/2\r\n\
             a=ssrc:12345 cname:test\r\n",
            ufrag
        )
    }

    fn make_bundle_offer() -> String {
        "v=0\r\n\
         o=- 123 2 IN IP4 0.0.0.0\r\n\
         s=-\r\n\
         t=0 0\r\n\
         a=group:BUNDLE 0 1\r\n\
         m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
         c=IN IP4 0.0.0.0\r\n\
         a=mid:0\r\n\
         a=ice-ufrag:cufrag\r\n\
         a=ice-pwd:cpwd\r\n\
         a=setup:actpass\r\n\
         a=sendrecv\r\n\
         a=rtcp-mux\r\n\
         a=rtpmap:111 opus/48000/2\r\n\
         a=extmap:4 urn:ietf:params:rtp-hdrext:sdes:mid\r\n\
         m=video 9 UDP/TLS/RTP/SAVPF 96 97\r\n\
         c=IN IP4 0.0.0.0\r\n\
         a=mid:1\r\n\
         a=ice-ufrag:cufrag\r\n\
         a=ice-pwd:cpwd\r\n\
         a=setup:actpass\r\n\
         a=sendrecv\r\n\
         a=rtcp-mux\r\n\
         a=rtcp-rsize\r\n\
         a=rtpmap:96 VP8/90000\r\n\
         a=rtcp-fb:96 transport-cc\r\n\
         a=rtpmap:97 rtx/90000\r\n\
         a=fmtp:97 apt=96\r\n\
         a=extmap:4 urn:ietf:params:rtp-hdrext:sdes:mid\r\n\
         a=ssrc:55555 cname:client\r\n\
         a=ssrc:55556 cname:client\r\n"
            .to_string()
    }

    fn make_inactive_offer() -> String {
        "v=0\r\n\
         o=- 123 2 IN IP4 0.0.0.0\r\n\
         s=-\r\n\
         t=0 0\r\n\
         a=group:BUNDLE 0 1\r\n\
         m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
         c=IN IP4 0.0.0.0\r\n\
         a=mid:0\r\n\
         a=sendrecv\r\n\
         a=rtcp-mux\r\n\
         a=rtpmap:111 opus/48000/2\r\n\
         m=audio 0 UDP/TLS/RTP/SAVPF 111\r\n\
         c=IN IP4 0.0.0.0\r\n\
         a=mid:1\r\n\
         a=inactive\r\n\
         a=rtcp-mux\r\n\
         a=rtpmap:111 opus/48000/2\r\n"
            .to_string()
    }

    // ── parse_offer tests ──

    #[test]
    fn parse_audio_offer() {
        let offer = make_audio_offer("cu");
        let parsed = parse_offer(&offer).unwrap();
        assert_eq!(parsed.sections.len(), 1);
        assert_eq!(parsed.sections[0].mid, "0");
        assert_eq!(parsed.sections[0].media_type, "audio");
        assert_eq!(parsed.sections[0].direction, "sendrecv");
        assert_eq!(parsed.bundle_mids, vec!["0"]);
    }

    #[test]
    fn parse_bundle_offer() {
        let offer = make_bundle_offer();
        let parsed = parse_offer(&offer).unwrap();
        assert_eq!(parsed.sections.len(), 2);
        assert_eq!(parsed.sections[0].media_type, "audio");
        assert_eq!(parsed.sections[1].media_type, "video");
        assert_eq!(parsed.bundle_mids, vec!["0", "1"]);
    }

    #[test]
    fn parse_strips_client_ice() {
        let offer = make_audio_offer("clientufrag");
        let parsed = parse_offer(&offer).unwrap();
        for line in &parsed.sections[0].codec_lines {
            assert!(!line.contains("clientufrag"), "client ICE should be stripped");
            assert!(!line.contains("clientpwd"), "client pwd should be stripped");
            assert!(!line.contains("AA:BB"), "client fingerprint should be stripped");
        }
    }

    #[test]
    fn parse_captures_ssrc() {
        let offer = make_audio_offer("cu");
        let parsed = parse_offer(&offer).unwrap();
        assert_eq!(parsed.sections[0].ssrcs, vec![12345]);
    }

    #[test]
    fn parse_captures_multiple_ssrcs() {
        let offer = make_bundle_offer();
        let parsed = parse_offer(&offer).unwrap();
        assert_eq!(parsed.sections[1].ssrcs, vec![55555, 55556]);
    }

    #[test]
    fn parse_ssrc_not_in_codec_lines() {
        let offer = make_bundle_offer();
        let parsed = parse_offer(&offer).unwrap();
        for sec in &parsed.sections {
            for line in &sec.codec_lines {
                assert!(!line.starts_with("a=ssrc"), "ssrc should not be in codec_lines");
            }
        }
    }

    #[test]
    fn parse_captures_direction() {
        let offer = make_inactive_offer();
        let parsed = parse_offer(&offer).unwrap();
        assert_eq!(parsed.sections[0].direction, "sendrecv");
        assert_eq!(parsed.sections[1].direction, "inactive");
    }

    #[test]
    fn parse_captures_rtcp_rsize() {
        let offer = make_bundle_offer();
        let parsed = parse_offer(&offer).unwrap();
        assert!(!parsed.sections[0].has_rtcp_rsize, "audio has no rtcp-rsize");
        assert!(parsed.sections[1].has_rtcp_rsize, "video has rtcp-rsize");
    }

    #[test]
    fn parse_preserves_extmap() {
        let offer = make_bundle_offer();
        let parsed = parse_offer(&offer).unwrap();
        let has_extmap = parsed.sections[0]
            .codec_lines
            .iter()
            .any(|l| l.contains("a=extmap:4"));
        assert!(has_extmap, "extmap should be preserved");
    }

    #[test]
    fn parse_empty_offer_fails() {
        let result = parse_offer("v=0\r\ns=-\r\n");
        assert!(result.is_err());
    }

    // ── build_answer tests ──

    #[test]
    fn answer_has_server_ice() {
        let offer = make_audio_offer("cu");
        let parsed = parse_offer(&offer).unwrap();
        let sdp = build_answer(&parsed, "sha-256 FF:00", "srvufrag", "srvpwd", 40000);
        assert!(sdp.contains("a=ice-ufrag:srvufrag"));
        assert!(sdp.contains("a=ice-pwd:srvpwd"));
    }

    #[test]
    fn answer_has_passive_setup() {
        let offer = make_audio_offer("cu");
        let parsed = parse_offer(&offer).unwrap();
        let sdp = build_answer(&parsed, "sha-256 FF:00", "u", "p", 40000);
        assert!(sdp.contains("a=setup:passive"));
    }

    #[test]
    fn answer_has_ice_lite() {
        let offer = make_audio_offer("cu");
        let parsed = parse_offer(&offer).unwrap();
        let sdp = build_answer(&parsed, "sha-256 FF:00", "u", "p", 40000);
        assert!(sdp.contains("a=ice-lite"));
    }

    #[test]
    fn answer_replaces_port() {
        let offer = make_audio_offer("cu");
        let parsed = parse_offer(&offer).unwrap();
        let sdp = build_answer(&parsed, "sha-256 FF:00", "u", "p", 41234);
        assert!(sdp.contains("m=audio 41234 "));
    }

    #[test]
    fn answer_has_server_fingerprint() {
        let offer = make_audio_offer("cu");
        let parsed = parse_offer(&offer).unwrap();
        let fp = "sha-256 AB:CD:EF";
        let sdp = build_answer(&parsed, fp, "u", "p", 40000);
        assert!(sdp.contains(&format!("a=fingerprint:{}", fp)));
        // offer의 fingerprint는 없어야 함
        assert!(!sdp.contains("AA:BB"));
    }

    #[test]
    fn answer_has_host_candidate() {
        let offer = make_audio_offer("cu");
        let parsed = parse_offer(&offer).unwrap();
        let sdp = build_answer(&parsed, "sha-256 FF:00", "u", "p", 40000);
        assert!(sdp.contains("typ host"));
        assert!(sdp.contains("a=end-of-candidates"));
    }

    #[test]
    fn answer_mirrors_codecs() {
        let offer = make_audio_offer("cu");
        let parsed = parse_offer(&offer).unwrap();
        let sdp = build_answer(&parsed, "sha-256 FF:00", "u", "p", 40000);
        assert!(sdp.contains("a=rtpmap:111 opus/48000/2"));
    }

    #[test]
    fn answer_direction_echoes_offer() {
        let offer = make_audio_offer("cu");
        let parsed = parse_offer(&offer).unwrap();
        let sdp = build_answer(&parsed, "sha-256 FF:00", "u", "p", 40000);
        assert!(sdp.contains("a=sendrecv"));
    }

    #[test]
    fn answer_strips_client_ice() {
        let offer = make_audio_offer("clientufrag");
        let parsed = parse_offer(&offer).unwrap();
        let sdp = build_answer(&parsed, "sha-256 FF:00", "srvufrag", "srvpwd", 40000);
        assert!(!sdp.contains("clientufrag"));
        assert!(!sdp.contains("clientpwd"));
    }

    #[test]
    fn answer_no_ssrc() {
        let offer = make_audio_offer("cu");
        let parsed = parse_offer(&offer).unwrap();
        let sdp = build_answer(&parsed, "sha-256 FF:00", "u", "p", 40000);
        assert!(!sdp.contains("a=ssrc:"), "answer should not echo ssrc");
    }

    #[test]
    fn answer_no_msid() {
        let offer = make_bundle_offer();
        let parsed = parse_offer(&offer).unwrap();
        let sdp = build_answer(&parsed, "sha-256 FF:00", "u", "p", 40000);
        assert!(!sdp.contains("a=msid:"), "answer should not have msid");
    }

    #[test]
    fn bundle_answer_two_sections() {
        let offer = make_bundle_offer();
        let parsed = parse_offer(&offer).unwrap();
        let sdp = build_answer(&parsed, "sha-256 FF:00", "u", "p", 40000);
        assert_eq!(sdp.matches("m=audio").count(), 1);
        assert_eq!(sdp.matches("m=video").count(), 1);
        assert!(sdp.contains("a=group:BUNDLE 0 1"));
    }

    #[test]
    fn bundle_answer_shared_ice() {
        let offer = make_bundle_offer();
        let parsed = parse_offer(&offer).unwrap();
        let sdp = build_answer(&parsed, "sha-256 FF:00", "srvufrag", "srvpwd", 40000);
        assert_eq!(sdp.matches("a=ice-ufrag:srvufrag").count(), 2);
        assert_eq!(sdp.matches("a=ice-pwd:srvpwd").count(), 2);
    }

    #[test]
    fn bundle_answer_preserves_extmap() {
        let offer = make_bundle_offer();
        let parsed = parse_offer(&offer).unwrap();
        let sdp = build_answer(&parsed, "sha-256 FF:00", "u", "p", 40000);
        let count = sdp.matches("a=extmap:4 urn:ietf:params:rtp-hdrext:sdes:mid").count();
        assert_eq!(count, 2, "extmap should be in both sections");
    }

    #[test]
    fn bundle_answer_rtcp_rsize_only_video() {
        let offer = make_bundle_offer();
        let parsed = parse_offer(&offer).unwrap();
        let sdp = build_answer(&parsed, "sha-256 FF:00", "u", "p", 40000);
        assert_eq!(sdp.matches("a=rtcp-rsize").count(), 1, "only video has rtcp-rsize");
    }

    #[test]
    fn inactive_direction_preserved() {
        let offer = make_inactive_offer();
        let parsed = parse_offer(&offer).unwrap();
        let sdp = build_answer(&parsed, "sha-256 FF:00", "u", "p", 40000);
        assert!(sdp.contains("a=inactive"));
        assert!(sdp.contains("a=sendrecv"));
    }

    // ── utility tests ──

    #[test]
    fn replace_port_works() {
        let m = "m=audio 9 UDP/TLS/RTP/SAVPF 111";
        assert_eq!(replace_m_line_port(m, 40000), "m=audio 40000 UDP/TLS/RTP/SAVPF 111");
    }

    #[test]
    fn replace_port_preserves_multiple_pts() {
        let m = "m=video 9 UDP/TLS/RTP/SAVPF 96 97 102 103";
        let result = replace_m_line_port(m, 19740);
        assert!(result.starts_with("m=video 19740 "));
        assert!(result.ends_with("96 97 102 103"));
    }

    #[test]
    fn detect_local_ip_returns_valid() {
        let ip = detect_local_ip();
        assert!(!ip.is_empty());
        assert!(ip.parse::<std::net::IpAddr>().is_ok());
    }
}
