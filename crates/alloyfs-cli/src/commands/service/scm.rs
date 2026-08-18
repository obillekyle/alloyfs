//! Talking to the Windows service control manager.

use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;

use windows_service::service::{
    ServiceAccess, ServiceErrorControl, ServiceInfo, ServiceStartType, ServiceState, ServiceType,
};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

use super::instance::service_name;

fn manager(access: ServiceManagerAccess) -> anyhow::Result<ServiceManager> {
    ServiceManager::local_computer(None::<&str>, access)
        .map_err(|e| anyhow::anyhow!("cannot reach the service control manager: {e}"))
}

/// Register `alloyfs-<id>` to run `alloyfs service run <id>` at boot.
///
/// LocalSystem, deliberately: the service needs `SE_TCB_NAME` to borrow the
/// console user's token, and only LocalSystem has it. It does not mount
/// anything itself — see `spawn` for why that distinction is the whole design.
pub fn create(id: &str) -> anyhow::Result<()> {
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
        "Launches an AlloyFS instance ({id}) into the interactive session. Managed by \
         `alloyfs service`; edit with that rather than services.msc."
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

/// Refuse early when WinFsp is missing.
///
/// Without it a mount fails at boot, inside a service, with nothing on screen —
/// the least debuggable moment available. Checking at `setup` and `add` time
/// puts the error in front of somebody who is watching.
pub fn verify_winfsp() -> anyhow::Result<()> {
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
pub fn restrict_to_administrators(dir: &Path) -> anyhow::Result<()> {
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
