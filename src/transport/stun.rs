// author: kodeholic (powered by Claude)
//! Minimal STUN parser/builder for ICE-Lite (RFC 8489)
//!
//! STUN message structure:
//!   0                   1                   2                   3
//!   0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
//!  +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//!  |0 0|     STUN Message Type     |         Message Length        |
//!  +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//!  |                         Magic Cookie                         |
//!  +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//!  |                                                               |
//!  |                     Transaction ID (96 bits)                  |
//!  |                                                               |
//!  +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//!
//! Header: 20 bytes
//! Attributes: TLV (Type: 2 bytes, Length: 2 bytes, Value: variable, padded to 4 bytes)

use std::net::SocketAddr;

/// STUN magic cookie (RFC 8489)
pub const MAGIC_COOKIE: u32 = 0x2112_A442;

/// STUN header size
pub const HEADER_SIZE: usize = 20;

// --- Message Types ---
// Type encoding: 0b00_MMMM_M_C_MMM_C (M=method bits, C=class bits)
// Binding method = 0x001
pub const BINDING_REQUEST: u16 = 0x0001;
pub const BINDING_RESPONSE: u16 = 0x0101;
pub const BINDING_ERROR_RESPONSE: u16 = 0x0111;

// --- Attribute Types ---
pub const ATTR_MAPPED_ADDRESS: u16 = 0x0001;
pub const ATTR_USERNAME: u16 = 0x0006;
pub const ATTR_MESSAGE_INTEGRITY: u16 = 0x0008;
pub const ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;
pub const ATTR_PRIORITY: u16 = 0x0024;
pub const ATTR_USE_CANDIDATE: u16 = 0x0025;
pub const ATTR_ICE_CONTROLLED: u16 = 0x8029;
pub const ATTR_ICE_CONTROLLING: u16 = 0x802A;
pub const ATTR_FINGERPRINT: u16 = 0x8028;
pub const ATTR_SOFTWARE: u16 = 0x8022;

// --- Address family ---
const ADDR_FAMILY_IPV4: u8 = 0x01;
const ADDR_FAMILY_IPV6: u8 = 0x02;

/// Parsed STUN message (zero-copy where possible)
#[derive(Debug)]
pub struct StunMessage<'a> {
    pub msg_type: u16,
    pub length: u16,
    pub transaction_id: [u8; 12],
    pub attributes: Vec<StunAttribute<'a>>,
    /// Raw bytes (for MESSAGE-INTEGRITY calculation)
    pub raw: &'a [u8],
}

#[derive(Debug)]
pub struct StunAttribute<'a> {
    pub attr_type: u16,
    pub value: &'a [u8],
}

/// Parse a STUN message from raw bytes
pub fn parse(buf: &[u8]) -> Option<StunMessage<'_>> {
    if buf.len() < HEADER_SIZE {
        return None;
    }

    // First 2 bits must be 0
    if buf[0] & 0xC0 != 0 {
        return None;
    }

    let msg_type = u16::from_be_bytes([buf[0], buf[1]]);
    let length = u16::from_be_bytes([buf[2], buf[3]]);
    let cookie = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);

    if cookie != MAGIC_COOKIE {
        return None;
    }

    // Length must be multiple of 4
    if length % 4 != 0 {
        return None;
    }

    let total_len = HEADER_SIZE + length as usize;
    if buf.len() < total_len {
        return None;
    }

    let mut transaction_id = [0u8; 12];
    transaction_id.copy_from_slice(&buf[8..20]);

    // Parse attributes
    let mut attrs = Vec::new();
    let mut offset = HEADER_SIZE;
    while offset + 4 <= total_len {
        let attr_type = u16::from_be_bytes([buf[offset], buf[offset + 1]]);
        let attr_len = u16::from_be_bytes([buf[offset + 2], buf[offset + 3]]) as usize;
        offset += 4;

        if offset + attr_len > total_len {
            break;
        }

        attrs.push(StunAttribute {
            attr_type,
            value: &buf[offset..offset + attr_len],
        });

        // Advance past value + padding (align to 4 bytes)
        offset += (attr_len + 3) & !3;
    }

    Some(StunMessage {
        msg_type,
        length,
        transaction_id,
        attributes: attrs,
        raw: &buf[..total_len],
    })
}

impl<'a> StunMessage<'a> {
    /// Find an attribute by type
    pub fn get_attr(&self, attr_type: u16) -> Option<&StunAttribute<'a>> {
        self.attributes.iter().find(|a| a.attr_type == attr_type)
    }

    /// Get USERNAME attribute as string (used for ICE ufrag matching)
    pub fn username(&self) -> Option<&str> {
        self.get_attr(ATTR_USERNAME)
            .and_then(|a| std::str::from_utf8(a.value).ok())
    }

    /// Check if USE-CANDIDATE attribute is present
    pub fn has_use_candidate(&self) -> bool {
        self.get_attr(ATTR_USE_CANDIDATE).is_some()
    }

    /// Get PRIORITY attribute value
    pub fn priority(&self) -> Option<u32> {
        self.get_attr(ATTR_PRIORITY).and_then(|a| {
            if a.value.len() == 4 {
                Some(u32::from_be_bytes([a.value[0], a.value[1], a.value[2], a.value[3]]))
            } else {
                None
            }
        })
    }

    /// Get MESSAGE-INTEGRITY attribute (20 bytes HMAC-SHA1)
    pub fn message_integrity(&self) -> Option<&[u8]> {
        self.get_attr(ATTR_MESSAGE_INTEGRITY).map(|a| a.value)
    }
}

/// Build a STUN Binding Success Response
///
/// Includes: XOR-MAPPED-ADDRESS, MESSAGE-INTEGRITY, FINGERPRINT
pub fn build_binding_response(
    transaction_id: &[u8; 12],
    remote_addr: SocketAddr,
    integrity_key: &[u8],
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(128);

    // --- Header (placeholder length, will be updated) ---
    buf.extend_from_slice(&BINDING_RESPONSE.to_be_bytes());
    buf.extend_from_slice(&0u16.to_be_bytes()); // length placeholder
    buf.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
    buf.extend_from_slice(transaction_id);

    // --- XOR-MAPPED-ADDRESS ---
    let xma = encode_xor_mapped_address(remote_addr, transaction_id);
    write_attr(&mut buf, ATTR_XOR_MAPPED_ADDRESS, &xma);

    // --- SOFTWARE (optional, short) ---
    let sw = b"light-livechat";
    write_attr(&mut buf, ATTR_SOFTWARE, sw);

    // --- MESSAGE-INTEGRITY ---
    // Update length field to include MESSAGE-INTEGRITY (24 bytes: 4 header + 20 value)
    let len_before_mi = buf.len() - HEADER_SIZE + 24; // 24 = MI attr size
    buf[2..4].copy_from_slice(&(len_before_mi as u16).to_be_bytes());

    let hmac_value = compute_hmac_sha1(&buf, integrity_key);
    write_attr(&mut buf, ATTR_MESSAGE_INTEGRITY, &hmac_value);

    // --- FINGERPRINT ---
    // Update length to include FINGERPRINT (8 bytes: 4 header + 4 value)
    let len_before_fp = buf.len() - HEADER_SIZE + 8;
    buf[2..4].copy_from_slice(&(len_before_fp as u16).to_be_bytes());

    let crc = crc32fast::hash(&buf) ^ 0x5354_554E; // XOR with STUN magic
    write_attr(&mut buf, ATTR_FINGERPRINT, &crc.to_be_bytes());

    // Final length update
    let final_len = (buf.len() - HEADER_SIZE) as u16;
    buf[2..4].copy_from_slice(&final_len.to_be_bytes());

    buf
}

/// Verify MESSAGE-INTEGRITY of a received STUN message
pub fn verify_message_integrity(msg: &StunMessage<'_>, key: &[u8]) -> bool {
    let Some(received_hmac) = msg.message_integrity() else {
        return false;
    };

    // Find the offset of MESSAGE-INTEGRITY attribute in raw bytes
    // We need to compute HMAC over the message up to (but not including) MI attribute
    // with the length field adjusted to include MI
    let mi_attr_offset = find_attr_offset(msg.raw, ATTR_MESSAGE_INTEGRITY);
    let Some(mi_offset) = mi_attr_offset else {
        return false;
    };

    // Create a copy with adjusted length
    let mut tmp = msg.raw[..mi_offset].to_vec();
    // Length field = (mi_offset - HEADER_SIZE) + 24 (MI attr size)
    let adjusted_len = (mi_offset - HEADER_SIZE + 24) as u16;
    tmp[2..4].copy_from_slice(&adjusted_len.to_be_bytes());

    let expected = compute_hmac_sha1(&tmp, key);
    // Constant-time comparison
    expected.len() == received_hmac.len()
        && expected
            .iter()
            .zip(received_hmac.iter())
            .all(|(a, b)| a == b)
}

/// Compute ICE integrity key from ufrag and password
/// For ICE, the key is simply the password of the peer being authenticated
pub fn ice_integrity_key(password: &str) -> Vec<u8> {
    password.as_bytes().to_vec()
}

// --- Internal helpers ---

fn write_attr(buf: &mut Vec<u8>, attr_type: u16, value: &[u8]) {
    buf.extend_from_slice(&attr_type.to_be_bytes());
    buf.extend_from_slice(&(value.len() as u16).to_be_bytes());
    buf.extend_from_slice(value);
    // Pad to 4-byte boundary
    let padding = (4 - (value.len() % 4)) % 4;
    buf.extend(std::iter::repeat(0u8).take(padding));
}

fn encode_xor_mapped_address(addr: SocketAddr, transaction_id: &[u8; 12]) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.push(0x00); // reserved

    match addr {
        SocketAddr::V4(v4) => {
            buf.push(ADDR_FAMILY_IPV4);
            let xport = v4.port() ^ (MAGIC_COOKIE >> 16) as u16;
            buf.extend_from_slice(&xport.to_be_bytes());
            let ip_bytes = v4.ip().octets();
            let cookie_bytes = MAGIC_COOKIE.to_be_bytes();
            for i in 0..4 {
                buf.push(ip_bytes[i] ^ cookie_bytes[i]);
            }
        }
        SocketAddr::V6(v6) => {
            buf.push(ADDR_FAMILY_IPV6);
            let xport = v6.port() ^ (MAGIC_COOKIE >> 16) as u16;
            buf.extend_from_slice(&xport.to_be_bytes());
            let ip_bytes = v6.ip().octets();
            let mut xor_key = [0u8; 16];
            xor_key[0..4].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
            xor_key[4..16].copy_from_slice(transaction_id);
            for i in 0..16 {
                buf.push(ip_bytes[i] ^ xor_key[i]);
            }
        }
    }
    buf
}

fn compute_hmac_sha1(data: &[u8], key: &[u8]) -> [u8; 20] {
    use hmac::{Hmac, Mac};
    use sha1::Sha1;

    type HmacSha1 = Hmac<Sha1>;
    let mut mac = HmacSha1::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data);
    let result = mac.finalize();
    let bytes = result.into_bytes();
    let mut out = [0u8; 20];
    out.copy_from_slice(&bytes);
    out
}

fn find_attr_offset(raw: &[u8], target_type: u16) -> Option<usize> {
    let mut offset = HEADER_SIZE;
    let total = raw.len();
    while offset + 4 <= total {
        let attr_type = u16::from_be_bytes([raw[offset], raw[offset + 1]]);
        if attr_type == target_type {
            return Some(offset);
        }
        let attr_len = u16::from_be_bytes([raw[offset + 2], raw[offset + 3]]) as usize;
        offset += 4 + ((attr_len + 3) & !3);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_binding_request() {
        // Minimal STUN Binding Request (header only, no attributes)
        let mut buf = vec![0u8; 20];
        // Type: Binding Request (0x0001)
        buf[0] = 0x00;
        buf[1] = 0x01;
        // Length: 0
        buf[2] = 0x00;
        buf[3] = 0x00;
        // Magic Cookie
        buf[4..8].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
        // Transaction ID
        buf[8..20].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);

        let msg = parse(&buf).unwrap();
        assert_eq!(msg.msg_type, BINDING_REQUEST);
        assert_eq!(msg.transaction_id, [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
        assert!(msg.attributes.is_empty());
    }

    #[test]
    fn test_parse_bad_cookie() {
        let mut buf = vec![0u8; 20];
        buf[0] = 0x00;
        buf[1] = 0x01;
        buf[4..8].copy_from_slice(&0xDEADBEEFu32.to_be_bytes());
        assert!(parse(&buf).is_none());
    }

    #[test]
    fn test_parse_too_short() {
        assert!(parse(&[0u8; 10]).is_none());
        assert!(parse(&[]).is_none());
    }

    #[test]
    fn test_build_and_parse_response() {
        let tid = [1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];
        let addr: SocketAddr = "192.168.1.1:12345".parse().unwrap();
        let key = ice_integrity_key("testpassword");

        let resp = build_binding_response(&tid, addr, &key);
        let msg = parse(&resp).unwrap();

        assert_eq!(msg.msg_type, BINDING_RESPONSE);
        assert_eq!(msg.transaction_id, tid);
        assert!(msg.get_attr(ATTR_XOR_MAPPED_ADDRESS).is_some());
        assert!(msg.get_attr(ATTR_MESSAGE_INTEGRITY).is_some());
        assert!(msg.get_attr(ATTR_FINGERPRINT).is_some());
    }

    #[test]
    fn test_message_integrity_verify() {
        let tid = [1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];
        let addr: SocketAddr = "10.0.0.1:5000".parse().unwrap();
        let key = ice_integrity_key("mypassword");

        let resp = build_binding_response(&tid, addr, &key);
        let msg = parse(&resp).unwrap();

        assert!(verify_message_integrity(&msg, &key));
        assert!(!verify_message_integrity(&msg, b"wrongpassword"));
    }

    #[test]
    fn test_xor_mapped_address_ipv4() {
        let tid = [0u8; 12];
        let addr: SocketAddr = "192.0.2.1:32853".parse().unwrap();
        let encoded = encode_xor_mapped_address(addr, &tid);

        // Family should be IPv4
        assert_eq!(encoded[1], ADDR_FAMILY_IPV4);
        // XOR'd port: 32853 ^ (0x2112 >> 0) = 32853 ^ 0x2112
        let xport = u16::from_be_bytes([encoded[2], encoded[3]]);
        assert_eq!(xport, 32853 ^ 0x2112);
    }
}
