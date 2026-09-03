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
    /// P4-T01: PLAYBACK_CMD arrived with a per-sender
    /// `monotonic_seq` that is `> last_acked_seq + 1`
    /// (a gap). The caller dropped one or more earlier
    /// commands; the server cannot apply this one
    /// without the missing range. Maps to
    /// `RoomErrorCode::StaleCommand` (wire string
    /// `"stale_command"`). The wire reply is
    /// single-caller; the command is NOT broadcast.
    #[error("playback monotonic_seq gap (got {got}, expected {expected})")]
    StaleCommand { got: u64, expected: u64 },
    /// P4-T01: PLAYBACK_CMD arrived with a per-sender
    /// `monotonic_seq` that is `<= last_acked_seq` (a
    /// duplicate). Maps to
    /// `RoomErrorCode::StaleCommand`.
    #[error("playback command is stale (seq {got} <= last_acked_seq {last})")]
    DuplicateCommand { got: u64, last: u64 },
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
            RoomError::StaleCommand { .. } => RoomErrorCode::StaleCommand,
            RoomError::DuplicateCommand { .. } => RoomErrorCode::StaleCommand,
            RoomError::Internal(_) => RoomErrorCode::Internal,
        }
    }
}
