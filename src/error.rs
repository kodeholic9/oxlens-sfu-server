// author: kodeholic (powered by Claude)
//! Error types for light-livechat

use thiserror::Error;

#[derive(Debug, Error)]
pub enum LightError {
    // 1xxx: Connection / Auth
    #[error("[1001] not authenticated")]
    NotAuthenticated,
    #[error("[1002] invalid token")]
    InvalidToken,
    #[error("[1003] already identified")]
    AlreadyIdentified,

    // 2xxx: Room
    #[error("[2001] room not found")]
    RoomNotFound,
    #[error("[2002] room full")]
    RoomFull,
    #[error("[2003] already in room")]
    AlreadyInRoom,
    #[error("[2004] not in room")]
    NotInRoom,
    #[error("[2005] room name required")]
    RoomNameRequired,

    // 3xxx: Signaling
    #[error("[3001] invalid opcode")]
    InvalidOpcode,
    #[error("[3002] invalid payload")]
    InvalidPayload,
    #[error("[3003] missing pid")]
    MissingPid,

    // 4xxx: Media / Transport
    #[error("[4001] sdp parse error")]
    SdpParseError,
    #[error("[4002] dtls handshake failed")]
    DtlsHandshakeFailed,
    #[error("[4003] srtp error")]
    SrtpError,

    // 9xxx: Internal
    #[error("[9001] internal error: {0}")]
    Internal(String),
}

impl LightError {
    pub fn code(&self) -> u16 {
        match self {
            Self::NotAuthenticated => 1001,
            Self::InvalidToken => 1002,
            Self::AlreadyIdentified => 1003,

            Self::RoomNotFound => 2001,
            Self::RoomFull => 2002,
            Self::AlreadyInRoom => 2003,
            Self::NotInRoom => 2004,
            Self::RoomNameRequired => 2005,

            Self::InvalidOpcode => 3001,
            Self::InvalidPayload => 3002,
            Self::MissingPid => 3003,

            Self::SdpParseError => 4001,
            Self::DtlsHandshakeFailed => 4002,
            Self::SrtpError => 4003,

            Self::Internal(_) => 9001,
        }
    }
}

pub type LightResult<T> = Result<T, LightError>;
