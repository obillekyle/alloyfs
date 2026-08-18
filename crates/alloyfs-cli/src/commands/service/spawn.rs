//! Launching an instance INTO the interactive session, as its user.
//!
//! The service itself runs as LocalSystem in session 0, and must not mount
//! anything there. Three separate things break if it does:
//!
//! * **File access.** The WinFsp backend reports no security descriptor and
//!   lets WinFsp synthesize one from the mounting user. Mounted by SYSTEM,
//!   every file carries a SYSTEM-derived descriptor, and an ordinary
//!   non-elevated process — bun, an editor, Explorer — is access-checked
//!   against it.
//! * **Visibility.** A drive letter created without Mount Manager is
//!   session-local, so a session-0 mount is not on the desktop at all.
//! * **Credentials.** An `ssh://` mount running as SYSTEM looks for SYSTEM's
//!   keys and agent, which do not exist.
//!
//! So the service resolves the console session, borrows that user's token,
//! swaps it for the un-filtered token behind it, and starts the real process
//! there. The child then runs as the user, in the user's session, elevated,
//! and with no console — every property the mount needs, including the Mount
//! Manager registration that keeps bun from seeing spurious ENOENT.

use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;

use windows_sys::Win32::Foundation::{CloseHandle, ERROR_NO_TOKEN, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Security::{
    DuplicateTokenEx, GetTokenInformation, SecurityIdentification, TokenLinkedToken, TokenPrimary,
    TOKEN_ALL_ACCESS, TOKEN_LINKED_TOKEN,
};
use windows_sys::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows_sys::Win32::System::RemoteDesktop::{WTSGetActiveConsoleSessionId, WTSQueryUserToken};
use windows_sys::Win32::System::Threading::{
    CreateProcessAsUserW, CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, PROCESS_INFORMATION, STARTUPINFOW,
};

/// A HANDLE that closes itself.
///
/// Several calls below can fail after an earlier handle is already open, and a
/// service leaking a token per retry is a slow invisible failure rather than a
/// loud one.
pub struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            unsafe { CloseHandle(self.0) };
        }
    }
}

impl Handle {
    /// Borrow the raw handle, for waiting on a spawned child.
    pub fn raw(&self) -> HANDLE {
        self.0
    }
}

/// Why no process was started. The caller treats these very differently, so
/// they stay distinct rather than collapsing into one error string.
pub enum SpawnError {
    /// Nobody is logged in at the console. Normal at boot and at the logon
    /// screen: the service waits for a session instead of failing.
    NoSession,
    /// Something actually went wrong.
    Failed(anyhow::Error),
}

impl From<anyhow::Error> for SpawnError {
    fn from(e: anyhow::Error) -> Self {
        Self::Failed(e)
    }
}

impl std::fmt::Display for SpawnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoSession => f.write_str("no interactive session"),
            Self::Failed(e) => write!(f, "{e}"),
        }
    }
}

fn wide(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s).encode_wide().chain(Some(0)).collect()
}

fn last_error() -> std::io::Error {
    std::io::Error::last_os_error()
}

/// The token of whoever is at the physical console, unwrapped to its elevated
/// form where one exists.
fn console_user_token() -> Result<Handle, SpawnError> {
    let session = unsafe { WTSGetActiveConsoleSessionId() };
    // 0xFFFFFFFF: no session is attached to the console.
    if session == u32::MAX {
        return Err(SpawnError::NoSession);
    }

    let mut raw: HANDLE = std::ptr::null_mut();
    if unsafe { WTSQueryUserToken(session, &mut raw) } == 0 {
        let err = last_error();
        // At the logon screen there is a session but no user inside it.
        if err.raw_os_error() == Some(ERROR_NO_TOKEN as i32) {
            return Err(SpawnError::NoSession);
        }
        return Err(anyhow::anyhow!("WTSQueryUserToken(session {session}): {err}").into());
    }

    Ok(elevate(Handle(raw)))
}

/// Swap a UAC-filtered token for the full one behind it.
///
/// Returns the token unchanged when there is no linked one, which happens for
/// two very different reasons: UAC is off, so the token is already full and
/// this is fine; or the user is not an administrator, so there is no elevated
/// token to get and it is not fine. Nothing here can tell those apart, so the
/// warning names the consequence rather than guessing at the cause.
fn elevate(token: Handle) -> Handle {
    let mut linked = TOKEN_LINKED_TOKEN {
        LinkedToken: std::ptr::null_mut(),
    };
    let mut size = 0u32;
    let ok = unsafe {
        GetTokenInformation(
            token.0,
            TokenLinkedToken,
            &mut linked as *mut _ as *mut c_void,
            std::mem::size_of::<TOKEN_LINKED_TOKEN>() as u32,
            &mut size,
        )
    };
    if ok == 0 || linked.LinkedToken.is_null() {
        tracing::warn!(
            "no elevated token for the console user: the mount will fall back to a session-local \
             drive letter, which breaks GetFinalPathNameByHandle round-trips and makes bun (and \
             similar tools) report spurious ENOENT. Is this account an administrator?"
        );
        return token;
    }
    Handle(linked.LinkedToken)
}

/// A primary token, which is what `CreateProcessAsUser` requires. The token
/// from `WTSQueryUserToken` is already primary; the linked one is not.
fn as_primary(token: &Handle) -> anyhow::Result<Handle> {
    let mut out: HANDLE = std::ptr::null_mut();
    let ok = unsafe {
        DuplicateTokenEx(
            token.0,
            TOKEN_ALL_ACCESS,
            std::ptr::null(),
            SecurityIdentification,
            TokenPrimary,
            &mut out,
        )
    };
    anyhow::ensure!(ok != 0, "DuplicateTokenEx: {}", last_error());
    Ok(Handle(out))
}

/// Quote one argument for a Windows command line.
///
/// Quoting everything unconditionally is simpler than deciding what needs it,
/// and the arguments that reach here are paths, urls and globs — all of which
/// can contain spaces, none of which should be re-split by the child.
fn quote(arg: &str) -> String {
    format!("\"{}\"", arg.replace('"', "\\\""))
}

/// Start `exe args...` as the console user, in their session, with no window.
///
/// Returns the child's process handle so the caller can wait on it: a mount
/// that exits is a mount that needs restarting, and the service is the only
/// thing positioned to notice.
pub fn spawn_in_console_session(exe: &std::path::Path, args: &[String]) -> Result<Handle, SpawnError> {
    let token = console_user_token()?;
    let primary = as_primary(&token).map_err(SpawnError::from)?;

    // The user's environment, not SYSTEM's. This is what puts their PATH,
    // USERPROFILE and SSH agent in scope; without it an ssh:// mount looks for
    // keys under C:\Windows\system32\config\systemprofile and finds none.
    let mut env: *mut c_void = std::ptr::null_mut();
    if unsafe { CreateEnvironmentBlock(&mut env, primary.raw(), 0) } == 0 {
        return Err(anyhow::anyhow!("CreateEnvironmentBlock: {}", last_error()).into());
    }

    let mut cmdline = quote(&exe.display().to_string());
    for arg in args {
        cmdline.push(' ');
        cmdline.push_str(&quote(arg));
    }
    let mut cmdline_w = wide(&cmdline);

    let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
    si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    // The interactive window station and desktop. Left unset, the child starts
    // in the service's invisible one, where a drive letter would not belong to
    // the desktop the user is looking at.
    let mut desktop = wide("winsta0\\default");
    si.lpDesktop = desktop.as_mut_ptr();

    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        CreateProcessAsUserW(
            primary.raw(),
            std::ptr::null(),
            cmdline_w.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT,
            env,
            std::ptr::null(),
            &si,
            &mut pi,
        )
    };
    // Capture before anything else can overwrite it.
    let err = last_error();
    unsafe { DestroyEnvironmentBlock(env) };
    if ok == 0 {
        return Err(anyhow::anyhow!("CreateProcessAsUserW: {err}").into());
    }

    // The thread handle is of no use here; the process handle is how the
    // caller waits for the child.
    unsafe { CloseHandle(pi.hThread) };
    tracing::info!(pid = pi.dwProcessId, cmdline, "launched into the console session");
    Ok(Handle(pi.hProcess))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Arguments reach the child through one flat string, so a path with a
    /// space has to survive the trip. `--exclude "My Docs"` re-split into two
    /// arguments would silently exclude the wrong thing.
    #[test]
    fn arguments_survive_spaces_and_quotes() {
        assert_eq!(quote("simple"), "\"simple\"");
        assert_eq!(quote("C:/My Docs/x"), "\"C:/My Docs/x\"");
        assert_eq!(quote("say \"hi\""), "\"say \\\"hi\\\"\"");
    }
}
