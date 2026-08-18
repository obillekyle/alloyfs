//! `alloyfs service` — mounts and agents that start on their own.
//!
//! Two rules shape everything here.
//!
//! **No terminal.** The managed process is launched with `CREATE_NO_WINDOW`
//! into the interactive session, so nothing flashes at logon.
//!
//! **No elevation prompt.** Every subcommand that touches the service control
//! manager requires an already-elevated shell and says so plainly. A tool that
//! silently re-launches itself through `runas` teaches people to click through
//! a UAC dialog they did not ask for, which is a worse habit than an error
//! message. Elevation is not optional cosmetics either: without it the mount
//! cannot register with the Mount Manager, and a session-local drive letter
//! breaks `GetFinalPathNameByHandle` round-trips — the reason bun reports
//! spurious ENOENT on this class of drive.

pub mod instance;

#[cfg(windows)]
pub mod runtime;
#[cfg(windows)]
pub mod spawn;

#[cfg(windows)]
mod scm;

pub use instance::Instance;

/// Everything except `list` refuses to run unelevated.
#[cfg(windows)]
pub fn require_elevation() -> anyhow::Result<()> {
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

#[cfg(not(windows))]
pub fn require_elevation() -> anyhow::Result<()> {
    Ok(())
}

/// Everything on this platform routes through here.
#[cfg(not(windows))]
fn unsupported() -> anyhow::Result<()> {
    anyhow::bail!(
        "`alloyfs service` is Windows-only for now.\n\
         \n\
         On Linux, install the agent as a systemd unit with:\n\
         \x20 curl -fsSL https://alloy.okyle.dev/service.sh | sudo sh"
    )
}

// ------------------------------------------------------------------ commands

pub fn setup() -> anyhow::Result<()> {
    #[cfg(not(windows))]
    return unsupported();
    #[cfg(windows)]
    {
        require_elevation()?;
        scm::verify_winfsp()?;
        let dir = instance::store_dir();
        std::fs::create_dir_all(&dir)?;
        scm::restrict_to_administrators(&dir)?;
        println!("ready.");
        println!("  instances: {}", dir.display());
        println!();
        println!("Add one with:");
        println!("  alloyfs service add work --url ssh://host/projects --mount P:");
        Ok(())
    }
}

pub fn add(id: String, instance: Instance, start_now: bool) -> anyhow::Result<()> {
    #[cfg(not(windows))]
    {
        let _ = (id, instance, start_now);
        return unsupported();
    }
    #[cfg(windows)]
    {
        require_elevation()?;
        instance::validate_id(&id)?;
        anyhow::ensure!(
            !instance::instance_path(&id).exists(),
            "service {id:?} already exists; remove it first"
        );
        scm::verify_winfsp()?;
        warn_about_ssh(&instance);

        instance::save(&id, &instance)?;
        scm::create(&id)?;
        println!("added {id}: {}", instance.summary());
        if start_now {
            scm::start(&id)?;
            println!("started.");
        } else {
            println!("It will start at boot. To start it now: alloyfs service start {id}");
        }
        Ok(())
    }
}

pub fn remove(id: String) -> anyhow::Result<()> {
    #[cfg(not(windows))]
    {
        let _ = id;
        return unsupported();
    }
    #[cfg(windows)]
    {
        require_elevation()?;
        instance::validate_id(&id)?;
        let _ = scm::stop(&id);
        scm::delete(&id)?;
        let path = instance::instance_path(&id);
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        println!("removed {id}");
        Ok(())
    }
}

/// `start`, `stop` and `restart` all fan out the same way: one id, or every
/// instance when none is given.
pub fn control(action: &str, id: Option<String>) -> anyhow::Result<()> {
    #[cfg(not(windows))]
    {
        let _ = (action, id);
        return unsupported();
    }
    #[cfg(windows)]
    {
        require_elevation()?;
        let ids = match id {
            Some(one) => {
                instance::validate_id(&one)?;
                vec![one]
            }
            None => instance::list_ids(),
        };
        anyhow::ensure!(
            !ids.is_empty(),
            "no services defined; add one with `alloyfs service add`"
        );

        // Keep going after a failure and report at the end: stopping four of
        // five drives and aborting silently on the last is worse than saying
        // which one refused.
        let mut failed = Vec::new();
        for one in &ids {
            let outcome = match action {
                "start" => scm::start(one),
                "stop" => scm::stop(one),
                "restart" => scm::stop(one).and_then(|()| scm::start(one)),
                other => anyhow::bail!("unknown action {other}"),
            };
            match outcome {
                Ok(()) => println!("{action}ed {one}"),
                Err(e) => {
                    eprintln!("{one}: {e}");
                    failed.push(one.clone());
                }
            }
        }
        anyhow::ensure!(failed.is_empty(), "failed for: {}", failed.join(", "));
        Ok(())
    }
}

pub fn list() -> anyhow::Result<()> {
    #[cfg(not(windows))]
    return unsupported();
    #[cfg(windows)]
    {
        // Deliberately readable without elevation: "what is registered" is a
        // question worth answering from any shell.
        let ids = instance::list_ids();
        if ids.is_empty() {
            println!("no services defined.");
            println!("  alloyfs service add work --url ssh://host/projects --mount P:");
            return Ok(());
        }
        println!("ID               KIND    STATE      TARGET");
        for id in ids {
            let (kind, target) = match instance::load(&id) {
                Ok(i) => (i.kind().to_string(), i.summary()),
                Err(e) => ("?".into(), format!("unreadable: {e}")),
            };
            let state = scm::state(&id);
            println!("{id:<16} {kind:<7} {state:<10} {target}");
        }
        Ok(())
    }
}

pub fn reset(confirm: bool) -> anyhow::Result<()> {
    #[cfg(not(windows))]
    {
        let _ = confirm;
        return unsupported();
    }
    #[cfg(windows)]
    {
        require_elevation()?;
        let ids = instance::list_ids();
        if !confirm {
            println!("This removes every AlloyFS service and its definition:");
            if ids.is_empty() {
                println!("  (nothing is defined)");
            }
            for id in &ids {
                println!("  {id}");
            }
            println!();
            anyhow::bail!("re-run with --confirm to proceed");
        }
        for id in &ids {
            let _ = scm::stop(id);
            if let Err(e) = scm::delete(id) {
                eprintln!("{id}: {e}");
            }
        }
        let dir = instance::store_dir();
        if dir.exists() {
            std::fs::remove_dir_all(&dir)?;
        }
        println!("removed {} service(s).", ids.len());
        Ok(())
    }
}

/// A LocalSystem service launches into the user's session with the user's
/// environment, so their SSH keys are the ones in scope. Worth confirming out
/// loud, because the opposite arrangement is the usual one and it fails at
/// boot for a reason nobody connects to SSH agent scope.
#[cfg(windows)]
fn warn_about_ssh(instance: &Instance) {
    if let Instance::Mount { url, .. } = instance {
        if url.starts_with("ssh://") {
            println!(
                "note: this mount authenticates as you, using your own SSH keys and agent.\n\
                 \x20     Key-based auth must work non-interactively — check with `ssh <host> true`;\n\
                 \x20     a key with a passphrase and no agent will hang at boot."
            );
        }
    }
}
