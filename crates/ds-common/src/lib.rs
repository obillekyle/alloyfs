//! Helpers shared by the agent (server) and client: exclude matching and the
//! small std::fs bridges. One copy — the server and client MUST agree on
//! exclude semantics (they sit on opposite ends of the wire), and this crate
//! is what makes drift impossible.

pub mod coalesce;
mod exclude;
mod fs;

pub use exclude::ExcludeSet;
pub use fs::{attr_from_metadata, read_at, read_fully, set_mode, write_at, write_fully};

use ds_proto::ErrorCode;

/// "2M" → 2 MiB, "512K", "1G", bare digits = bytes, "0" disables.
pub fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty size".into());
    }
    let (digits, mult) = match s.chars().last().unwrap().to_ascii_uppercase() {
        'K' => (&s[..s.len() - 1], 1024u64),
        'M' => (&s[..s.len() - 1], 1024 * 1024),
        'G' => (&s[..s.len() - 1], 1024 * 1024 * 1024),
        c if c.is_ascii_digit() => (s, 1),
        c => return Err(format!("unknown size suffix {c:?} in {s:?}")),
    };
    digits
        .parse::<u64>()
        .map_err(|e| format!("bad size {s:?}: {e}"))?
        .checked_mul(mult)
        .ok_or_else(|| format!("size {s:?} overflows"))
}

/// Config-file size value: `2M` (string) or `2097152` (integer bytes).
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(untagged)]
pub enum SizeField {
    Bytes(u64),
    Human(String),
}

impl SizeField {
    pub fn to_bytes(&self) -> Result<u64, String> {
        match self {
            SizeField::Bytes(n) => Ok(*n),
            SizeField::Human(s) => parse_size(s),
        }
    }
}

/// Is `listen` (a "host:port" listen address) loopback-only? Resolves the
/// address rather than string-matching, so "127.0.0.2:80" and IPv6 forms are
/// judged correctly. Unresolvable input counts as NON-loopback — the callers
/// use this to decide whether serving without a token is safe, and "can't
/// tell" must fail toward requiring one.
pub fn is_loopback_listen(listen: &str) -> bool {
    use std::net::ToSocketAddrs;
    match listen.to_socket_addrs() {
        Ok(mut addrs) => addrs.all(|a| a.ip().is_loopback()),
        Err(_) => false,
    }
}

/// Constant-time token equality: never leak how much of a secret matched
/// through response timing. Used by both the HTTP bearer check and the TCP
/// `Request::Auth` handler.
pub fn token_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_size_forms() {
        assert_eq!(parse_size("0"), Ok(0));
        assert_eq!(parse_size("42"), Ok(42));
        assert_eq!(parse_size("2M"), Ok(2 * 1024 * 1024));
        assert_eq!(parse_size("512k"), Ok(512 * 1024));
        assert_eq!(parse_size("1G"), Ok(1024 * 1024 * 1024));
        assert!(parse_size("").is_err());
        assert!(parse_size("2X").is_err());
        assert!(parse_size("M").is_err());
    }
}
