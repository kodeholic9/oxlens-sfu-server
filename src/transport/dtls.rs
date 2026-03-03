// author: kodeholic (powered by Claude)
//! DTLS passive handshake + SRTP key derivation
//!
//! RFC 5764 §4.2 key material layout (AES_CM_128_HMAC_SHA1_80, 60 bytes):
//!   [0..16]   client_write_key  (16)  — inbound  (browser → server)
//!   [16..32]  server_write_key  (16)  — outbound (server → browser)
//!   [32..46]  client_write_salt (14)
//!   [46..60]  server_write_salt (14)

use std::sync::Arc;
use sha2::{Digest, Sha256};
use tracing::{debug, info};
use dtls::config::Config as DtlsConfig;
use dtls::config::ExtendedMasterSecretType;
use dtls::conn::DTLSConn;
use dtls::crypto::Certificate;
use dtls::extension::extension_use_srtp::SrtpProtectionProfile;
use webrtc_util::conn::Conn;
use webrtc_util::KeyingMaterialExporter;

// RFC 5764 §4.2 constants
const SRTP_KEY_LABEL:        &str  = "EXTRACTOR-dtls_srtp";
const SRTP_MASTER_KEY_LEN:   usize = 16;
const SRTP_MASTER_SALT_LEN:  usize = 14;
const SRTP_KEY_MATERIAL_LEN: usize = (SRTP_MASTER_KEY_LEN + SRTP_MASTER_SALT_LEN) * 2; // 60

// ============================================================================
// Server certificate
// ============================================================================

pub struct ServerCert {
    pub dtls_cert:   Certificate,
    pub fingerprint: String,
}

impl ServerCert {
    pub fn generate() -> Result<Self, dtls::Error> {
        let dtls_cert = Certificate::generate_self_signed(
            vec!["light-livechat".to_string()],
        )?;

        let fingerprint = sha256_fingerprint(
            dtls_cert.certificate.first().map(|c| c.as_ref()).unwrap_or(&[]),
        );
        info!("DTLS server cert generated fingerprint={:.47}...", fingerprint);

        Ok(Self { dtls_cert, fingerprint })
    }
}

// ============================================================================
// DTLS config
// ============================================================================

pub fn server_config(cert: &ServerCert) -> DtlsConfig {
    DtlsConfig {
        certificates: vec![cert.dtls_cert.clone()],
        srtp_protection_profiles: vec![
            SrtpProtectionProfile::Srtp_Aes128_Cm_Hmac_Sha1_80,
        ],
        extended_master_secret: ExtendedMasterSecretType::Require,
        insecure_skip_verify: true,
        ..Default::default()
    }
}

// ============================================================================
// Handshake
// ============================================================================

pub async fn accept_dtls(
    conn:   Arc<dyn Conn + Send + Sync>,
    config: DtlsConfig,
) -> Result<DTLSConn, dtls::Error> {
    info!("starting DTLS handshake (server mode)");

    let dtls_conn = DTLSConn::new(
        conn,
        config,
        false, // is_client = false (passive)
        None,
    )
    .await?;

    info!("DTLS handshake completed");
    Ok(dtls_conn)
}

// ============================================================================
// SRTP key derivation (RFC 5705 → RFC 5764 §4.2)
// ============================================================================

pub struct SrtpKeyMaterial {
    pub client_key:  Vec<u8>,
    pub server_key:  Vec<u8>,
    pub client_salt: Vec<u8>,
    pub server_salt: Vec<u8>,
}

/// Extract SRTP keying material from completed DTLS connection.
///
/// DTLSConn::connection_state() → State::export_keying_material()
/// context param MUST be &[] (empty), otherwise ContextUnsupported error.
pub async fn export_srtp_keys(
    dtls_conn: &DTLSConn,
) -> Result<SrtpKeyMaterial, Box<dyn std::error::Error + Send + Sync>> {
    let state = dtls_conn.connection_state().await;
    let material: Vec<u8> = state
        .export_keying_material(SRTP_KEY_LABEL, &[], SRTP_KEY_MATERIAL_LEN)
        .await
        .map_err(|e| format!("export_keying_material failed: {e:?}"))?;

    debug!("extracted {} bytes of SRTP keying material", material.len());

    let client_key  = material[0..SRTP_MASTER_KEY_LEN].to_vec();
    let server_key  = material[SRTP_MASTER_KEY_LEN..SRTP_MASTER_KEY_LEN * 2].to_vec();
    let client_salt = material[SRTP_MASTER_KEY_LEN * 2..SRTP_MASTER_KEY_LEN * 2 + SRTP_MASTER_SALT_LEN].to_vec();
    let server_salt = material[SRTP_MASTER_KEY_LEN * 2 + SRTP_MASTER_SALT_LEN..].to_vec();

    Ok(SrtpKeyMaterial { client_key, server_key, client_salt, server_salt })
}

// ============================================================================
// Utility
// ============================================================================

fn sha256_fingerprint(der: &[u8]) -> String {
    let hash = Sha256::digest(der);
    let hex: Vec<String> = hash.iter().map(|b| format!("{:02X}", b)).collect();
    format!("sha-256 {}", hex.join(":"))
}
