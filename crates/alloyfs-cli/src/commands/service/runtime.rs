//! The service side: what runs under the SCM, and what it supervises.
//!
//! This process does nothing useful itself. It waits for an interactive
//! session, launches a plain `alloyfs start` (or `alloyfs mount <name>`) into
//! it via [`super::spawn`], and keeps it alive. The child is an ordinary CLI
//! invocation — the same one `alloyfs service list` prints — so anything that
//! goes wrong can be reproduced by hand without the service in the way.
//!
//! It never reads the config. The instance file says which command to run, and
//! the child — running as the logged-in user — is the one that opens the
//! config that command names.

use std::ffi::OsString;
use std::sync::mpsc;
use std::time::Duration;

use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
    SessionChangeReason,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
use windows_sys::Win32::System::Threading::{
    OpenProcess, TerminateProcess, WaitForSingleObject, PROCESS_TERMINATE,
};

use super::spawn::{self, SpawnError};

/// How long to wait before retrying after a failed launch, and how often to
/// re-check for a session when nobody is logged in.
const RETRY: Duration = Duration::from_secs(5);
/// A child that dies faster than this is failing, not finishing; back off so a
/// broken instance does not spin.
const TOO_QUICK: Duration = Duration::from_secs(10);

windows_service::define_windows_service!(ffi_service_main, service_main);

/// Entry point for `alloyfs service run <id>`.
///
/// Only the SCM may call this: `service_dispatcher::start` connects to the
/// control pipe the SCM sets up, and fails immediately anywhere else. That is
/// worth saying in the error, because running it by hand is the obvious thing
/// to try when a service will not start.
pub fn run(id: &str) -> anyhow::Result<()> {
    // The dispatcher gives the service main no useful arguments, so the id
    // travels out of band.
    std::env::set_var("ALLOYFS_SERVICE_ID", id);
    windows_service::service_dispatcher::start(super::instance::service_name(id), ffi_service_main).map_err(
        |e| {
            anyhow::anyhow!(
                "`service run` is what the Windows service manager invokes; it cannot be run \
                 directly ({e}). To start this instance use: alloyfs service start {id}"
            )
        },
    )
}

fn service_main(_args: Vec<OsString>) {
    let id = std::env::var("ALLOYFS_SERVICE_ID").unwrap_or_default();
    if let Err(e) = supervise(&id) {
        tracing::error!(error = %e, id, "service exited");
    }
}

/// What the supervisor is being told to do, from the SCM's control handler.
enum Signal {
    /// Stop the child and exit.
    Stop,
    /// A session appeared or vanished: re-evaluate what should be running.
    SessionChanged,
}

fn supervise(id: &str) -> anyhow::Result<()> {
    let instance = super::instance::load(id)?;
    let exe = std::env::current_exe()?;
    let argv = instance.argv();

    let (tx, rx) = mpsc::channel::<Signal>();
    let control_tx = tx.clone();
    let status_handle = service_control_handler::register(
        super::instance::service_name(id),
        move |control| -> ServiceControlHandlerResult {
            match control {
                ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
                ServiceControl::Stop | ServiceControl::Shutdown => {
                    let _ = control_tx.send(Signal::Stop);
                    ServiceControlHandlerResult::NoError
                }
                // Logon/logoff and fast user switching all change which
                // session the mount belongs in. Without acting on these, a
                // mount survives as a handle into a session that no longer
                // exists, and the next user gets nothing.
                ServiceControl::SessionChange(param) => {
                    match param.reason {
                        SessionChangeReason::SessionLogon
                        | SessionChangeReason::SessionLogoff
                        | SessionChangeReason::ConsoleConnect
                        | SessionChangeReason::ConsoleDisconnect => {
                            let _ = control_tx.send(Signal::SessionChanged);
                        }
                        _ => {}
                    }
                    ServiceControlHandlerResult::NoError
                }
                _ => ServiceControlHandlerResult::NotImplemented,
            }
        },
    )?;

    let report = |state: ServiceState, accept: ServiceControlAccept| ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted: accept,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    };
    let accepted = ServiceControlAccept::STOP | ServiceControlAccept::SESSION_CHANGE;
    status_handle.set_service_status(report(ServiceState::Running, accepted))?;

    let mut child: Option<spawn::Handle> = None;
    let mut started_at = std::time::Instant::now();

    loop {
        // Launch when nothing is running. A missing session is not a failure:
        // at boot the service starts before anyone logs in, and the right
        // behaviour is to wait quietly for one.
        if child.is_none() {
            match spawn::spawn_in_console_session(&exe, &argv) {
                Ok(handle) => {
                    child = Some(handle);
                    started_at = std::time::Instant::now();
                }
                Err(SpawnError::NoSession) => {
                    tracing::debug!(id, "no interactive session yet; waiting");
                }
                Err(SpawnError::Failed(e)) => {
                    tracing::error!(error = %e, id, "launch failed; retrying");
                }
            }
        }

        match rx.recv_timeout(RETRY) {
            Ok(Signal::Stop) => break,
            Ok(Signal::SessionChanged) => {
                // Whatever changed, the safe move is to drop the current child
                // and let the next pass decide afresh. Restarting a working
                // mount costs a reconnect; leaving one pointed at a dead
                // session costs the drive.
                if let Some(handle) = child.take() {
                    kill(&handle);
                }
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }

        // Has the child exited on its own?
        if let Some(handle) = &child {
            let done = unsafe { WaitForSingleObject(handle.raw(), 0) } == WAIT_OBJECT_0;
            if done {
                let lived = started_at.elapsed();
                child = None;
                if lived < TOO_QUICK {
                    tracing::warn!(
                        id,
                        secs = lived.as_secs(),
                        "the instance exited immediately; backing off before retrying"
                    );
                    std::thread::sleep(RETRY);
                } else {
                    tracing::info!(id, "the instance exited; restarting");
                }
            }
        }
    }

    if let Some(handle) = child.take() {
        kill(&handle);
    }
    status_handle.set_service_status(report(ServiceState::Stopped, ServiceControlAccept::empty()))?;
    Ok(())
}

/// End the child.
///
/// `TerminateProcess` rather than a polite signal: Windows has no SIGTERM, and
/// a mount holds no state worth flushing — the overlay and cache are written
/// as they go, and WinFsp tears the volume down when the process dies.
fn kill(child: &spawn::Handle) {
    // The handle from CreateProcessAsUser lacks PROCESS_TERMINATE, so reopen
    // by pid with the right access.
    let pid = unsafe { windows_sys::Win32::System::Threading::GetProcessId(child.raw()) };
    if pid == 0 {
        return;
    }
    let h = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
    if h.is_null() {
        tracing::warn!(pid, "could not open the instance process to stop it");
        return;
    }
    unsafe {
        TerminateProcess(h, 0);
        CloseHandle(h);
    }
    tracing::info!(pid, "instance stopped");
}
