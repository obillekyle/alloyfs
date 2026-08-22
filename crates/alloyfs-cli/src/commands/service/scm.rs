//! Talking to the Windows service control manager.
//!
//! This is one half of a pair: `systemd.rs` registers the same instances on
//! Linux and answers to the same function names, so the command bodies in the
//! parent module are written once. Where the two platforms genuinely differ —
//! elevation, the filesystem driver, who may write the instance store — the
//! difference is a function here with a counterpart there, not a `cfg` in the
//! middle of a command.

use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;

use windows_service::service::{
    ServiceAccess, ServiceErrorControl, ServiceExitCode, ServiceInfo, ServiceStartType, ServiceState,
    ServiceType,
};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

use super::instance::{service_name, Instance};

fn manager(access: ServiceManagerAccess) -> anyhow::Result<ServiceManager> {
    ServiceManager::local_computer(None::<&str>, access)
        .map_err(|e| anyhow::anyhow!("cannot reach the service control manager: {e}"))
}

/// Register `alloyfs-<id>` to run `alloyfs service run <id>` at boot.
///
/// LocalSystem, deliberately: the service needs `SE_TCB_NAME` to borrow the
/// console user's token, and only LocalSystem has it. It does not mount
/// anything itself — see `spawn` for why that distinction is the whole design.
///
/// The instance is not baked into the registration the way it is in a systemd
/// unit: what this launches is the supervisor, which reads the definition from
/// the store at start. It reaches the description, so `services.msc` says what
/// the instance actually runs.
pub fn create(id: &str, instance: &Instance) -> anyhow::Result<()> {
    let exe = std::env::current_exe()?;
    let info = ServiceInfo {
        name: OsString::from(service_name(id)),
        display_name: OsString::from(format!("AlloyFS ({id})")),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: exe,
        launch_arguments: vec![
            OsString::from("service"),
            OsString::from("run"),
            OsString::from(id),
        ],
        dependencies: vec![],
        account_name: None, // None == LocalSystem
        account_password: None,
    };
    let mgr = manager(ServiceManagerAccess::CREATE_SERVICE)?;
    let service = mgr
        .create_service(&info, ServiceAccess::CHANGE_CONFIG)
        .map_err(|e| anyhow::anyhow!("creating service {}: {e}", service_name(id)))?;
    let _ = service.set_description(format!(
        "Launches `alloyfs {}` into the interactive session. Managed by \
         `alloyfs service`; edit with that rather than services.msc.",
        instance.command()
    ));
    Ok(())
}

pub fn delete(id: &str) -> anyhow::Result<()> {
    let mgr = manager(ServiceManagerAccess::CONNECT)?;
    let service = mgr
        .open_service(service_name(id), ServiceAccess::DELETE)
        .map_err(|e| anyhow::anyhow!("no service {}: {e}", service_name(id)))?;
    service.delete()?;
    Ok(())
}

pub fn start(id: &str) -> anyhow::Result<()> {
    let mgr = manager(ServiceManagerAccess::CONNECT)?;
    let service = mgr.open_service(
        service_name(id),
        ServiceAccess::START | ServiceAccess::QUERY_STATUS,
    )?;
    if service.query_status()?.current_state == ServiceState::Running {
        return Ok(());
    }
    service.start::<&str>(&[])?;
    Ok(())
}

pub fn stop(id: &str) -> anyhow::Result<()> {
    let mgr = manager(ServiceManagerAccess::CONNECT)?;
    let service = mgr.open_service(
        service_name(id),
        ServiceAccess::STOP | ServiceAccess::QUERY_STATUS,
    )?;
    if service.query_status()?.current_state == ServiceState::Stopped {
        return Ok(());
    }
    service.stop()?;
    // Wait for it rather than returning into a race: `restart` starts again
    // immediately afterwards, and the SCM rejects a start while the previous
    // instance is still stopping.
    for _ in 0..50 {
        if service.query_status()?.current_state == ServiceState::Stopped {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    anyhow::bail!("{} did not stop within 5s", service_name(id))
}

/// A word for `service list`. Never fails: an unregistered or unreadable
/// service is information, not an error.
pub fn state(id: &str) -> String {
    let Ok(mgr) = manager(ServiceManagerAccess::CONNECT) else {
        return "?".into();
    };
    let Ok(service) = mgr.open_service(service_name(id), ServiceAccess::QUERY_STATUS) else {
        return "unregistered".into();
    };
    match service.query_status() {
        Ok(status) => match status.current_state {
            ServiceState::Stopped => "stopped".into(),
            ServiceState::StartPending => "starting".into(),
            ServiceState::StopPending => "stopping".into(),
            ServiceState::Running => "running".into(),
            ServiceState::ContinuePending => "resuming".into(),
            ServiceState::PausePending => "pausing".into(),
            ServiceState::Paused => "paused".into(),
        },
        Err(_) => "?".into(),
    }
}

/// Why a service is in the state it is in, when the SCM has anything to
/// say about it.
///
/// `state` answers one word, which is all a table column should hold — and
/// for a service that keeps dying that one word is "running", because the
/// supervisor is up and the CHILD is what is failing. The exit code is
/// what distinguishes a service that stopped because someone stopped it
/// from one that stopped because it could not start, and it was being
/// read out of the status struct and thrown away.
///
/// `None` when there is nothing worth saying: a healthy running service
/// with a clean last exit needs no annotation.
pub fn detail(id: &str) -> Option<String> {
    let mgr = manager(ServiceManagerAccess::CONNECT).ok()?;
    let service = mgr
        .open_service(service_name(id), ServiceAccess::QUERY_STATUS)
        .ok()?;
    let status = service.query_status().ok()?;
    let mut parts = Vec::new();
    match status.exit_code {
        // NO_ERROR is the resting value; reporting it would annotate every
        // healthy row with noise.
        ServiceExitCode::Win32(0) => {}
        ServiceExitCode::Win32(code) => parts.push(format!("last exit: win32 {code}")),
        ServiceExitCode::ServiceSpecific(code) => parts.push(format!("last exit: service-specific {code}")),
    }
    // The pid is deliberately NOT here. It is real information, but it is
    // present for every healthy service, and annotating every healthy row
    // would make the detail line mean "here is a number" instead of "here
    // is what went wrong" — which is the only reason to give up a line.
    (!parts.is_empty()).then(|| parts.join(", "))
}

/// Everything except `list` refuses to run unelevated.
///
/// Nothing here will raise its own privileges. A tool that silently re-launches
/// itself through `runas` teaches people to click through a UAC dialog they did
/// not ask for, which is a worse habit than an error message. Elevation is not
/// optional cosmetics either: without it the mount cannot register with the
/// Mount Manager, and a session-local drive letter breaks
/// `GetFinalPathNameByHandle` round-trips — the reason bun reports spurious
/// ENOENT on this class of drive.
///
/// The Linux counterpart of this function refuses the opposite thing. A systemd
/// user unit needs no privilege at all, so there `sudo` is the mistake.
pub fn preflight() -> anyhow::Result<()> {
    use std::ffi::c_void;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let mut token: HANDLE = std::ptr::null_mut();
    let opened = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };
    anyhow::ensure!(
        opened != 0,
        "OpenProcessToken: {}",
        std::io::Error::last_os_error()
    );

    let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut size = 0u32;
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            &mut elevation as *mut _ as *mut c_void,
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut size,
        )
    };
    unsafe { CloseHandle(token) };
    anyhow::ensure!(
        ok != 0,
        "GetTokenInformation: {}",
        std::io::Error::last_os_error()
    );

    anyhow::ensure!(
        elevation.TokenIsElevated != 0,
        "this needs an elevated terminal.\n\
         \n\
         Open PowerShell or Terminal with \"Run as administrator\" and try again. \
         alloyfs will not raise its own privileges: registering a service that runs \
         at boot is not something a tool should arrange on your behalf behind a \
         dialog you did not open."
    );
    Ok(())
}

/// The service control manager is part of the OS; there is nothing to check.
///
/// Its Linux counterpart has real work to do, because a box can be running
/// without a systemd user manager for the account in hand.
pub fn verify_supervisor() -> anyhow::Result<()> {
    Ok(())
}

/// Nothing here corresponds to systemd lingering: a Windows service registered
/// `AutoStart` starts at boot, full stop.
pub fn linger_note() -> Option<String> {
    None
}

/// Refuse early when WinFsp is missing.
///
/// Without it a mount fails at boot, inside a service, with nothing on screen —
/// the least debuggable moment available. Checking at `setup` and `add` time
/// puts the error in front of somebody who is watching.
pub fn verify_backend() -> anyhow::Result<()> {
    let installed = [
        r"C:\Program Files (x86)\WinFsp\bin\winfsp-x64.dll",
        r"C:\Program Files\WinFsp\bin\winfsp-x64.dll",
    ]
    .iter()
    .any(|p| Path::new(p).exists());
    anyhow::ensure!(
        installed,
        "WinFsp is not installed — a mount cannot work without it.\n\
         Install it from https://winfsp.dev and re-run this."
    );
    Ok(())
}

/// Lock the instance directory to SYSTEM and Administrators.
///
/// Whoever can write here decides what a LocalSystem service launches at boot,
/// which makes a writable-by-users directory a privilege escalation rather
/// than an untidiness. `icacls` rather than hand-built ACLs: this runs once,
/// during an already-elevated `setup`, and the shell-out is auditable at a
/// glance in a way a page of SID plumbing is not.
pub fn prepare_store(dir: &Path) -> anyhow::Result<()> {
    let out = std::process::Command::new("icacls")
        .arg(dir)
        .args([
            "/inheritance:r",
            "/grant:r",
            "*S-1-5-18:(OI)(CI)F", // SYSTEM
            "/grant:r",
            "*S-1-5-32-544:(OI)(CI)F", // Administrators
        ])
        .output()
        .map_err(|e| anyhow::anyhow!("running icacls: {e}"))?;
    anyhow::ensure!(
        out.status.success(),
        "could not restrict {}: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(())
}
