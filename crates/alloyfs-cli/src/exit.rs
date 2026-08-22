//! What the process exits with, and why it is worth distinguishing.
//!
//! Everything used to be 0, 1 or clap's 2. A wrapper script, a cron job or
//! a supervisor could tell "it worked" from "it did not" and nothing else —
//! so "the server is down for a minute" and "this config can never work"
//! were the same number, though one is worth retrying and the other never
//! is. That distinction is already load-bearing here: the generated
//! systemd unit reasons explicitly about on-failure versus always, and the
//! comment justifying it turns on which failures are transient.
//!
//! Codes are attached at the CLI boundary, where the meaning is known.
//! Lower layers keep returning ordinary errors — a transport does not know
//! whether its caller considers an unreachable peer fatal.

/// A failure that carries the exit code it deserves.
///
/// Displays as its message alone, so it reads like any other error; the
/// code is recovered by `main` walking the chain, which means a site can
/// add `.context(...)` on top without hiding it.
#[derive(Debug)]
pub struct Fatal {
    pub code: i32,
    pub message: String,
}

impl Fatal {
    /// Build the failure already wrapped, since every caller wants it as an
    /// `anyhow::Error`. Named `err` rather than `new` because it does not
    /// return `Self`, and a `new` that hands back something else is a small
    /// lie every reader has to check.
    pub fn err(code: i32, message: impl Into<String>) -> anyhow::Error {
        anyhow::Error::new(Fatal {
            code,
            message: message.into(),
        })
    }
}

impl std::fmt::Display for Fatal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Fatal {}

/// Something went wrong that has no more specific code. The default, and
/// what every un-annotated failure still exits with.
pub const GENERAL: i32 = 1;
// 2 is clap's: a usage error, printed by clap before anything here runs.
/// The configuration is missing, unreadable, or describes something
/// impossible. Retrying will not help; a human has to edit a file.
pub const CONFIG: i32 = 3;
/// The server could not be reached. The transient one — a host that is
/// rebooting or a link that is down produces this, and retrying is exactly
/// the right response.
pub const UNREACHABLE: i32 = 4;
/// The mountpoint cannot be used: a drive letter already in use, a
/// directory that is not empty, a mount that is already there.
pub const MOUNTPOINT: i32 = 5;
/// The filesystem driver is missing — WinFsp or FUSE. Needs an install,
/// not a retry.
pub const DRIVER: i32 = 6;
/// The command needs different privileges than it was given: elevation on
/// Windows, or NOT being under sudo on Linux.
pub const PRIVILEGE: i32 = 7;

/// The code an error chain asks for, or `GENERAL`.
pub fn code_of(err: &anyhow::Error) -> i32 {
    err.chain()
        .find_map(|cause| cause.downcast_ref::<Fatal>().map(|f| f.code))
        .unwrap_or(GENERAL)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_code_survives_context_added_on_top_of_it() {
        // The realistic shape: a low layer fails, a higher one explains
        // what it was doing. The explanation must not bury the code.
        let err = Fatal::err(UNREACHABLE, "connection refused").context("while mounting work");
        assert_eq!(code_of(&err), UNREACHABLE);
        assert!(format!("{err:#}").contains("connection refused"));
        assert!(format!("{err:#}").contains("while mounting work"));
    }

    #[test]
    fn an_ordinary_error_is_the_general_code() {
        assert_eq!(code_of(&anyhow::anyhow!("something")), GENERAL);
    }
}
