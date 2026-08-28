//! Closed room-error set. Maps to [`locast_protocol::room::RoomErrorCode`].

#![forbid(unsafe_code)]

use locast_protocol::room::RoomErrorCode;
use thiserror::Error;

/// Internal room errors. The dispatcher converts these into
/// wire-level `RoomErrorPayload` envelopes using the
/// `Into<RoomErrorCode>` conversion.
#[derive(Debug, Error)]
pub enum RoomError {
    #[error("not authenticated")]
    Unauthorized,
    #[error("invalid room code")]
    InvalidCode,
    #[error("room not found")]
    RoomNotFound,
    #[error("room is closed")]
    RoomClosed,
    #[error("room is full")]
    RoomFull,
    #[error("already joined")]
    AlreadyJoined,
    #[error("not joined")]
    NotJoined,
    #[error("invalid state for this action")]
    InvalidState,
    #[error("not host")]
    NotHost,
    #[error("host migration is disabled")]
    MigrationDisabled,
    #[error("internal error: {0}")]
    Internal(String),
}

impl From<RoomError> for RoomErrorCode {
    fn from(e: RoomError) -> Self {
        match e {
            RoomError::Unauthorized => RoomErrorCode::Unauthorized,
            RoomError::InvalidCode => RoomErrorCode::InvalidCode,
            RoomError::RoomNotFound => RoomErrorCode::RoomNotFound,
            RoomError::RoomClosed => RoomErrorCode::RoomClosed,
            RoomError::RoomFull => RoomErrorCode::RoomFull,
            RoomError::AlreadyJoined => RoomErrorCode::AlreadyJoined,
            RoomError::NotJoined => RoomErrorCode::NotJoined,
            RoomError::InvalidState => RoomErrorCode::InvalidState,
            RoomError::NotHost => RoomErrorCode::NotHost,
            RoomError::MigrationDisabled => RoomErrorCode::MigrationDisabled,
            RoomError::Internal(_) => RoomErrorCode::Internal,
        }
    }
}
