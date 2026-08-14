use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Errors that travel over the wire. Each maps cleanly onto both a Unix errno
/// and a Windows NTSTATUS in the mount backends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Error)]
pub enum ErrorCode {
    #[error("not found")]
    NotFound,
    #[error("permission denied")]
    PermissionDenied,
    #[error("already exists")]
    AlreadyExists,
    #[error("not a directory")]
    NotADirectory,
    #[error("is a directory")]
    IsADirectory,
    #[error("directory not empty")]
    NotEmpty,
    #[error("invalid path")]
    InvalidPath,
    #[error("invalid handle")]
    BadHandle,
    #[error("export is read-only")]
    ReadOnly,
    #[error("no such export")]
    NoSuchExport,
    #[error("would block (lock held)")]
    WouldBlock,
    #[error("write conflict")]
    Conflict,
    #[error("protocol version mismatch")]
    VersionMismatch,
    #[error("request not valid before Attach")]
    NotAttached,
    #[error("event history no longer available")]
    TooOld,
    #[error("server I/O error")]
    Io,
    /// Rename/link across the client overlay boundary. Client-generated only
    /// (no server sends it); appended last so postcard stays wire-compatible.
    #[error("cross-device link or rename")]
    CrossDevice,
    /// v3+: the session must send `Request::Auth` with the agent's tcp_token
    /// before anything else. Appended last (postcard append-only rule).
    #[error("authentication required")]
    AuthRequired,
}

/// Errors in the codec itself (framing/serialization) — these are fatal to a
/// connection, unlike `ErrorCode`s which are per-request outcomes.
#[derive(Debug, Error)]
pub enum ProtoError {
    #[error("frame of {0} bytes exceeds the {max} byte limit", max = crate::MAX_FRAME_LEN)]
    FrameTooLarge(usize),
    #[error("malformed frame: {0}")]
    Malformed(#[from] postcard::Error),
    #[error("bad compressed frame: {0}")]
    Compression(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
