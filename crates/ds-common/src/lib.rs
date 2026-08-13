//! Helpers shared by the agent (server) and client: exclude matching and the
//! small std::fs bridges. One copy — the server and client MUST agree on
//! exclude semantics (they sit on opposite ends of the wire), and this crate
//! is what makes drift impossible.

mod exclude;
mod fs;

pub use exclude::ExcludeSet;
pub use fs::{attr_from_metadata, read_at, read_fully, set_mode, write_at, write_fully};

use ds_proto::ErrorCode;

/// Map std::io errors onto wire error codes.
pub fn io_to_code(e: &std::io::Error) -> ErrorCode {
    use std::io::ErrorKind::*;
    match e.kind() {
        NotFound => ErrorCode::NotFound,
        PermissionDenied => ErrorCode::PermissionDenied,
        AlreadyExists => ErrorCode::AlreadyExists,
        NotADirectory => ErrorCode::NotADirectory,
        IsADirectory => ErrorCode::IsADirectory,
        DirectoryNotEmpty => ErrorCode::NotEmpty,
        _ => ErrorCode::Io,
    }
}

/// `io::Result<T> → Result<T, ErrorCode>` without the 40-character
/// `.map_err(|e| io_to_code(&e))` incantation at every call site.
pub trait OrCode<T> {
    fn or_code(self) -> Result<T, ErrorCode>;
}

impl<T> OrCode<T> for std::io::Result<T> {
    fn or_code(self) -> Result<T, ErrorCode> {
        self.map_err(|e| io_to_code(&e))
    }
}
