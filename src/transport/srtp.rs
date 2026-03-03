// author: kodeholic (powered by Claude)
//! SRTP context — encrypt/decrypt using webrtc-srtp
//!
//! Profile: AES_CM_128_HMAC_SHA1_80 (WebRTC standard, RFC 5764)
//!   master_key  = 16 bytes
//!   master_salt = 14 bytes

use tracing::debug;
use webrtc_srtp::context::Context;
use webrtc_srtp::protection_profile::ProtectionProfile;

// ============================================================================
// SrtpContext
// ============================================================================

pub struct SrtpContext {
    inner: Option<Context>,
}

impl SrtpContext {
    pub fn new() -> Self {
        Self { inner: None }
    }

    /// Install SRTP key (called after DTLS handshake completes)
    pub fn install_key(&mut self, key: &[u8], salt: &[u8]) {
        match Context::new(
            key,
            salt,
            ProtectionProfile::Aes128CmHmacSha1_80,
            None,
            None,
        ) {
            Ok(ctx) => {
                self.inner = Some(ctx);
                debug!("[srtp] context ready key_len={} salt_len={}", key.len(), salt.len());
            }
            Err(e) => {
                tracing::error!("[srtp] Context::new failed: {:?}", e);
            }
        }
    }

    pub fn is_ready(&self) -> bool {
        self.inner.is_some()
    }

    pub fn decrypt_rtp(&mut self, packet: &[u8]) -> Result<Vec<u8>, SrtpError> {
        match &mut self.inner {
            None => Err(SrtpError::KeyNotInstalled),
            Some(ctx) => ctx
                .decrypt_rtp(packet)
                .map(|b: bytes::Bytes| b.to_vec())
                .map_err(|e| SrtpError::DecryptFailed(e.to_string())),
        }
    }

    pub fn decrypt_rtcp(&mut self, packet: &[u8]) -> Result<Vec<u8>, SrtpError> {
        match &mut self.inner {
            None => Err(SrtpError::KeyNotInstalled),
            Some(ctx) => ctx
                .decrypt_rtcp(packet)
                .map(|b: bytes::Bytes| b.to_vec())
                .map_err(|e| SrtpError::DecryptFailed(e.to_string())),
        }
    }

    pub fn encrypt_rtp(&mut self, packet: &[u8]) -> Result<Vec<u8>, SrtpError> {
        match &mut self.inner {
            None => Err(SrtpError::KeyNotInstalled),
            Some(ctx) => ctx
                .encrypt_rtp(packet)
                .map(|b: bytes::Bytes| b.to_vec())
                .map_err(|e| SrtpError::EncryptFailed(e.to_string())),
        }
    }
}

impl Default for SrtpContext {
    fn default() -> Self { Self::new() }
}

// ============================================================================
// SrtpError
// ============================================================================

#[derive(Debug)]
pub enum SrtpError {
    DecryptFailed(String),
    EncryptFailed(String),
    KeyNotInstalled,
}

impl std::fmt::Display for SrtpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SrtpError::DecryptFailed(m) => write!(f, "SRTP decrypt failed: {}", m),
            SrtpError::EncryptFailed(m) => write!(f, "SRTP encrypt failed: {}", m),
            SrtpError::KeyNotInstalled  => write!(f, "SRTP key not installed"),
        }
    }
}

impl std::error::Error for SrtpError {}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_context_is_not_ready() {
        let ctx = SrtpContext::new();
        assert!(!ctx.is_ready());
    }

    #[test]
    fn key_install_marks_ready() {
        let mut ctx = SrtpContext::new();
        ctx.install_key(&[0u8; 16], &[0u8; 14]);
        assert!(ctx.is_ready());
    }

    #[test]
    fn decrypt_before_key_returns_error() {
        let mut ctx = SrtpContext::new();
        let rtp = [0x80, 0x78, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0xE2, 0x40];
        assert!(matches!(ctx.decrypt_rtp(&rtp), Err(SrtpError::KeyNotInstalled)));
    }

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let key  = [0x01u8; 16];
        let salt = [0x02u8; 14];

        let plaintext: Vec<u8> = vec![
            0x80, 0x78, 0x00, 0x01,
            0x00, 0x00, 0x00, 0x00,
            0x00, 0x01, 0xE2, 0x40,
            0xDE, 0xAD, 0xBE, 0xEF,
        ];

        let mut enc_ctx = SrtpContext::new();
        enc_ctx.install_key(&key, &salt);

        let mut dec_ctx = SrtpContext::new();
        dec_ctx.install_key(&key, &salt);

        let encrypted = enc_ctx.encrypt_rtp(&plaintext).expect("encrypt failed");
        let decrypted = dec_ctx.decrypt_rtp(&encrypted).expect("decrypt failed");

        assert_eq!(decrypted, plaintext);
    }
}
