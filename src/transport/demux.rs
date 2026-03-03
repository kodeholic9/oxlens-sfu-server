// author: kodeholic (powered by Claude)
//! Packet demultiplexer for single-port Bundle (RFC 5764)
//!
//! All traffic arrives on a single UDP port. The first byte determines the type:
//!   0x00..=0x03 → STUN
//!   0x14..=0x3F → DTLS
//!   0x80..=0xBF → SRTP/SRTCP

use crate::config;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketType {
    Stun,
    Dtls,
    Srtp,
    Unknown,
}

/// Classify a UDP packet by its first byte (RFC 5764 Section 5.1.2)
pub fn classify(buf: &[u8]) -> PacketType {
    if buf.is_empty() {
        return PacketType::Unknown;
    }
    let first = buf[0];
    match first {
        config::DEMUX_STUN_MIN..=config::DEMUX_STUN_MAX => PacketType::Stun,
        config::DEMUX_DTLS_MIN..=config::DEMUX_DTLS_MAX => PacketType::Dtls,
        config::DEMUX_RTP_MIN..=config::DEMUX_RTP_MAX => PacketType::Srtp,
        _ => PacketType::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_classify_stun() {
        assert_eq!(classify(&[0x00, 0x01]), PacketType::Stun);
        assert_eq!(classify(&[0x01, 0x01]), PacketType::Stun);
    }

    #[test]
    fn test_classify_dtls() {
        // DTLS ContentType: handshake=0x16, app_data=0x17
        assert_eq!(classify(&[0x16, 0xFE]), PacketType::Dtls);
        assert_eq!(classify(&[0x17, 0xFE]), PacketType::Dtls);
    }

    #[test]
    fn test_classify_srtp() {
        // RTP version 2: first byte 0x80
        assert_eq!(classify(&[0x80, 0x60]), PacketType::Srtp);
    }

    #[test]
    fn test_classify_empty() {
        assert_eq!(classify(&[]), PacketType::Unknown);
    }

    #[test]
    fn test_classify_unknown() {
        assert_eq!(classify(&[0x50]), PacketType::Unknown);
    }
}
