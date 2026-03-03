// author: kodeholic (powered by Claude)
//! ICE-Lite implementation
//!
//! ICE-Lite (RFC 8445 Section 2.2):
//! - Server is always the "controlled" agent
//! - Does NOT gather candidates or send connectivity checks
//! - Only responds to STUN Binding Requests from clients
//! - Single host candidate (server's listening UDP address)
//!
//! Flow:
//!   1. Client sends STUN Binding Request with USERNAME="server_ufrag:client_ufrag"
//!   2. Server verifies MESSAGE-INTEGRITY using server's password
//!   3. Server responds with Binding Success Response (XOR-MAPPED-ADDRESS)
//!   4. If USE-CANDIDATE is present, mark the candidate pair as selected

use std::net::SocketAddr;
use tracing::{debug, warn};

use crate::transport::stun;

/// ICE credentials for a session
#[derive(Debug, Clone)]
pub struct IceCredentials {
    pub ufrag: String,
    pub pwd: String,
}

impl IceCredentials {
    pub fn new() -> Self {
        let ufrag = random_ice_string(4);
        let pwd = random_ice_string(22);
        Self { ufrag, pwd }
    }
}

/// Result of processing a STUN packet in ICE-Lite mode
pub enum IceResult {
    /// Send this response back to the remote address
    SendResponse {
        data: Vec<u8>,
        remote: SocketAddr,
        use_candidate: bool,
    },
    /// Not a valid ICE packet, ignore
    Ignore,
}

/// Process an incoming STUN Binding Request (ICE-Lite server side)
///
/// - `buf`: raw UDP packet bytes
/// - `remote`: sender's address
/// - `local_creds`: this server's ICE credentials
pub fn handle_stun_packet(
    buf: &[u8],
    remote: SocketAddr,
    local_creds: &IceCredentials,
) -> IceResult {
    let Some(msg) = stun::parse(buf) else {
        return IceResult::Ignore;
    };

    // Only handle Binding Requests
    if msg.msg_type != stun::BINDING_REQUEST {
        debug!("ignoring non-binding STUN message: 0x{:04X}", msg.msg_type);
        return IceResult::Ignore;
    }

    // Verify USERNAME attribute: format "server_ufrag:client_ufrag"
    let Some(username) = msg.username() else {
        warn!("STUN binding request missing USERNAME");
        return IceResult::Ignore;
    };

    let expected_prefix = format!("{}:", local_creds.ufrag);
    if !username.starts_with(&expected_prefix) {
        warn!(
            "STUN USERNAME mismatch: expected prefix '{}', got '{}'",
            expected_prefix, username
        );
        return IceResult::Ignore;
    }

    // Verify MESSAGE-INTEGRITY using server's password
    let key = stun::ice_integrity_key(&local_creds.pwd);
    if !stun::verify_message_integrity(&msg, &key) {
        warn!("STUN MESSAGE-INTEGRITY verification failed");
        return IceResult::Ignore;
    }

    debug!(
        "ICE: valid binding request from {}, username={}, use_candidate={}",
        remote,
        username,
        msg.has_use_candidate()
    );

    // Build Binding Success Response
    let response = stun::build_binding_response(&msg.transaction_id, remote, &key);

    IceResult::SendResponse {
        data: response,
        remote,
        use_candidate: msg.has_use_candidate(),
    }
}

/// Generate a random ICE string (alphanumeric, a-z 0-9)
fn random_ice_string(len: usize) -> String {
    let mut bytes = vec![0u8; len];
    getrandom::fill(&mut bytes).expect("getrandom failed");

    bytes
        .iter()
        .map(|b| {
            let r = b % 36;
            if r < 10 {
                (b'0' + r) as char
            } else {
                (b'a' + r - 10) as char
            }
        })
        .collect()
}
