use std::path::PathBuf;
use std::sync::Arc;

use alloyfs_common::SizeField;

use crate::config::{cache_root, data_root, migrate_legacy_mount, parse_size, Config, ResolvedMount};
use crate::urls::{
    connect_target, dialer_for, export_key, host_key, legacy_mount_key, require_export, whoami,
};

/// Which driver serves the mount. FUSE is the default everywhere: the alloyfs
/// kernel module is not installed on most machines, and a default that needs
/// `modprobe` would break every existing mount command.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum Backend {
    #[default]
    Fuse,
    /// Linux only: the alloyfs kernel module, which can deliver remote changes
    /// as genuine inotify events.
    Kernel,
}

/// `alloyfs mount` as the command line spells it: a url plus a mountpoint, or
/// the name of a mount the config already describes.
///
/// Which form it is comes from how many positionals arrived, never from how the
/// first one looks. Sniffing for `://` would make `alloyfs mount work` depend on
/// whether the name happened to resemble a url, and every misjudgement would
/// surface as a connection failure naming a host nobody typed.
#[allow(clippy::too_many_arguments)]
pub async fn run_cli(
    target: String,
    mountpoint: Option<PathBuf>,
    remote_cmd: String,
    config: Option<PathBuf>,
    excludes: Vec<String>,
    pins: Vec<String>,
    auto_cache_max: Option<String>,
    auto_cache_budget: Option<String>,
    data_dir: Option<PathBuf>,
    no_server_defaults: bool,
    detect_conflicts: bool,
    token: Option<String>,
    backend: Backend,
) -> anyhow::Result<()> {
    if let Some(mountpoint) = mountpoint {
        return run(
            target,
            mountpoint,
            remote_cmd,
            config,
            excludes,
            pins,
            auto_cache_max,
            auto_cache_budget,
            data_dir,
            no_server_defaults,
            detect_conflicts,
            token,
            backend,
        )
        .await;
    }

    // One positional: a name. `--config` still decides WHICH file the name is
    // looked up in; without it, the usual discovery order applies.
    let cfg = crate::config::load_or_default(config)?;
    let mount = resolve_named(&cfg, &target)?;
    tracing::info!(
        mount = %target,
        url = %mount.url,
        at = %mount.at.display(),
        "mounting a configured mount"
    );

    // The file is deliberately not passed on to `run`: `mount` is already the
    // collapsed form of the `client:` defaults plus this entry, and reading it
    // a second time down there would put `client.exclude` back over a mount
    // that said `exclude: []` precisely to inherit nothing. What is left to
    // apply is the flags, and flags win.
    let excludes = if excludes.is_empty() {
        mount.exclude
    } else {
        excludes
    };
    let pins = if pins.is_empty() { mount.pin } else { pins };
    let auto_cache_max = resolve_size(auto_cache_max, mount.auto_cache_max, "auto_cache_max")?;
    let auto_cache_budget = resolve_size(auto_cache_budget, mount.auto_cache_budget, "auto_cache_budget")?;
    run(
        mount.url,
        mount.at,
        remote_cmd,
        None,
        excludes,
        pins,
        auto_cache_max,
        auto_cache_budget,
        data_dir.or(mount.data_dir),
        no_server_defaults || mount.no_server_defaults,
        detect_conflicts || mount.detect_conflicts,
        token.or(mount.token),
        backend,
    )
    .await
}

/// The mount `name` stands for, or an error that names the mounts which DO
/// exist.
///
/// A miss must not fall through to url parsing. `alloyfs mount wrok` would then
/// fail as an unknown scheme, or as a DNS lookup for a typo — an error about the
/// network, when the actual problem is a name that is not in the file.
fn resolve_named(cfg: &Config, name: &str) -> anyhow::Result<ResolvedMount> {
    if let Some(mount) = cfg.mount(name) {
        return Ok(mount);
    }
    let known = cfg.mount_names();
    if known.is_empty() {
        anyhow::bail!(
            "no mount named `{name}`: this config defines no mounts.\n\n\
             \x20 Add one under `client.mounts:`, or give a url and a mountpoint:\n\
             \x20   alloyfs mount tcp://host:7440/export <mountpoint>"
        );
    }
    anyhow::bail!(
        "no mount named `{name}`.\n\n\
         \x20 configured mounts: {}\n\n\
         \x20 alloyfs mount <name>              mounts one of those\n\
         \x20 alloyfs mount <url> <mountpoint>  mounts something the config does not describe",
        known.join(", ")
    )
}

/// One cache size as the command line finally means it: the flag when one was
/// given, otherwise whatever the config resolved to.
///
/// The configured value becomes a plain byte count rather than its original
/// spelling, because `run` parses whatever string it is handed and a resolved
/// `2M` and a resolved `2097152` have to reach it as the same number.
fn resolve_size(
    flag: Option<String>,
    configured: Option<SizeField>,
    key: &str,
) -> anyhow::Result<Option<String>> {
    match flag {
        Some(flag) => Ok(Some(flag)),
        None => match configured {
            Some(size) => {
                let bytes = size.to_bytes().map_err(|e| anyhow::anyhow!("{key}: {e}"))?;
                Ok(Some(bytes.to_string()))
            }
            None => Ok(None),
        },
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn run(
    url: String,
    mountpoint: PathBuf,
    remote_cmd: String,
    config: Option<PathBuf>,
    excludes: Vec<String>,
    pins: Vec<String>,
    auto_cache_max: Option<String>,
    auto_cache_budget: Option<String>,
    data_dir: Option<PathBuf>,
    no_server_defaults: bool,
    detect_conflicts: bool,
    token: Option<String>,
    backend: Backend,
) -> anyhow::Result<()> {
    // File values first, CLI flags override. Sizes stay None when neither
    // set one, so a server suggestion (proto v2+) can fill them in.
    //
    // The file may be a v3 config carrying a whole `client:` section, or one
    // of the flat pre-v3 mount configs; `crate::config::load` normalises both
    // (and upgrades the old shape on disk). What lands here is the `client:`
    // defaults, since a `--config` given to `mount` is asking for exactly
    // that: how this mount should behave.
    let file_cfg = match &config {
        Some(path) => crate::config::load(path)?.client.unwrap_or_default(),
        None => crate::config::ClientSection::default(),
    };
    let excludes = if excludes.is_empty() {
        file_cfg.exclude.unwrap_or_default()
    } else {
        excludes
    };
    let pins = if pins.is_empty() {
        file_cfg.pin.unwrap_or_default()
    } else {
        pins
    };
    let auto_cache_max = match (auto_cache_max, &file_cfg.auto_cache_max) {
        (Some(flag), _) => Some(parse_size(&flag).map_err(|e| anyhow::anyhow!(e))?),
        (None, Some(f)) => Some(f.to_bytes().map_err(|e| anyhow::anyhow!(e))?),
        (None, None) => None,
    };
    let auto_cache_budget = match (auto_cache_budget, &file_cfg.auto_cache_budget) {
        (Some(flag), _) => Some(parse_size(&flag).map_err(|e| anyhow::anyhow!(e))?),
        (None, Some(f)) => Some(f.to_bytes().map_err(|e| anyhow::anyhow!(e))?),
        (None, None) => None,
    };
    let no_server_defaults = no_server_defaults || file_cfg.no_server_defaults.unwrap_or(false);
    let token = token.or(file_cfg.token);

    let (conn, export) = connect_target(&url, &remote_cmd, &whoami(), token.as_deref()).await?;
    let export = require_export(export, &url)?;
    tracing::info!(server = conn.server_name, proto = conn.proto, "connected");

    // ~/.alloyfs/{data,cache}/<host>/, with --data-dir overriding the root.
    let host = host_key(&url);
    let export_key = export_key(&export);
    migrate_legacy_mount(&legacy_mount_key(&url, &export), &host, &export_key);
    let (data_dir, cache_dir) = match data_dir.or(file_cfg.data_dir) {
        Some(root) => (root.join("data").join(&host), root.join("cache").join(&host)),
        None => (data_root(&host), cache_root(&host)),
    };

    let opts = alloyfs_client::ClientOptions {
        mount_key: export_key,
        cache_dir,
        excludes,
        pins,
        auto_cache_max,
        auto_cache_budget,
        // CLI mounts default to auto-caching (2M files, 512M budget) when
        // neither the user nor the server picked values.
        auto_cache_max_fallback: 2 * 1024 * 1024,
        auto_cache_budget_fallback: 512 * 1024 * 1024,
        no_server_defaults,
        detect_conflicts: detect_conflicts || file_cfg.detect_conflicts.unwrap_or(false),
        // So the client can rewrite symlink targets that point back into
        // this mount (see RemoteFs::localize_target).
        mount_root: Some(mountpoint.to_string_lossy().into_owned()),
        // Survive connection loss: re-dial, re-attach, re-open handles,
        // resubscribe events. Locks do not survive (documented).
        dialer: Some(dialer_for(&url, &remote_cmd, &whoami(), token)),
        data_dir,
    };
    let fs = alloyfs_client::RemoteFs::attach_with(conn, &export, opts).await?;
    // Each platform starts the event pump itself, wiring server events into
    // its native notification mechanism.
    mount_platform(fs, mountpoint, &export, backend).await
}

#[cfg(unix)]
async fn mount_platform(
    fs: Arc<alloyfs_client::RemoteFs>,
    mountpoint: PathBuf,
    export: &str,
    backend: Backend,
) -> anyhow::Result<()> {
    match backend {
        Backend::Fuse => mount_fuse(fs, mountpoint, export).await,
        #[cfg(target_os = "linux")]
        Backend::Kernel => mount_kernel(fs, mountpoint, export).await,
        #[cfg(not(target_os = "linux"))]
        Backend::Kernel => anyhow::bail!("--backend kernel is Linux-only; use --backend fuse"),
    }
}

#[cfg(unix)]
async fn mount_fuse(
    fs: Arc<alloyfs_client::RemoteFs>,
    mountpoint: PathBuf,
    export: &str,
) -> anyhow::Result<()> {
    let export = export.to_string();
    let (notifier_tx, notifier_rx) = tokio::sync::oneshot::channel();
    let mount_fs = fs.clone();
    // The mount blocks its thread until unmount; the runtime stays free to
    // service the connection and the event pump.
    let mount_task = tokio::task::spawn_blocking(move || {
        alloyfs_mount_fuse::mount(mount_fs, &mountpoint, &export, move |notifier| {
            let _ = notifier_tx.send(notifier);
        })
    });
    if let Ok(notifier) = notifier_rx.await {
        // Server events → cache invalidation (inside the pump) → kernel cache
        // eviction, so the next stat/read/listing is instantly fresh.
        let pump_fs = fs.clone();
        fs.start_event_pump(move |batch| {
            alloyfs_mount_fuse::apply_events_native(&pump_fs, &notifier, batch);
        })
        .await?;
    }
    mount_task.await??;
    fs.shutdown(); // persist the auto-cache manifest
    Ok(())
}

/// The alloyfs kernel module. Same shape as the FUSE path — mount on a
/// blocking thread, then pump server events into it — except the events
/// become real fsnotify events inside the kernel instead of cache
/// invalidations.
#[cfg(target_os = "linux")]
async fn mount_kernel(
    fs: Arc<alloyfs_client::RemoteFs>,
    mountpoint: PathBuf,
    export: &str,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        alloyfs_mount_kernel::device_available(),
        "--backend kernel needs {} — the alloyfs kernel module is not loaded \
         (try: sudo modprobe alloyfs, or use --backend fuse)",
        alloyfs_mount_kernel::DEVICE
    );
    let export = export.to_string();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let mount_fs = fs.clone();
    let mount_task = tokio::task::spawn_blocking(move || {
        alloyfs_mount_kernel::mount(mount_fs, &mountpoint, &export, move |handle| {
            let _ = ready_tx.send(handle);
        })
    });
    let mut on_signal = None;
    if let Ok(handle) = ready_rx.await {
        let sink = handle.events();
        fs.start_event_pump(move |batch| sink.push(batch)).await?;
        // Unmount on Ctrl-C: unlike FUSE, a killed daemon here leaves a
        // mounted filesystem whose every operation fails until someone
        // umounts it by hand.
        on_signal = Some(tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                tracing::info!("unmounting");
                handle.shutdown();
            }
        }));
    }
    let result = mount_task.await?;
    if let Some(task) = on_signal {
        task.abort();
    }
    result?;
    fs.shutdown(); // persist the auto-cache manifest
    Ok(())
}

#[cfg(windows)]
async fn mount_platform(
    fs: Arc<alloyfs_client::RemoteFs>,
    mountpoint: PathBuf,
    export: &str,
    backend: Backend,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        backend == Backend::Fuse,
        "--backend kernel is Linux-only; Windows mounts always use WinFsp"
    );
    let mountpoint = mountpoint.to_string_lossy().into_owned();
    // mount() returns once the WinFsp dispatcher is running; its threads call
    // back into RemoteFs, which block_on's this runtime — so we keep the
    // runtime alive here until Ctrl-C, then unmount cleanly.
    let drive = alloyfs_mount_winfsp::mount(fs.clone(), &mountpoint, export)?;
    // Server events → cache invalidation (in the pump) → the notify timer
    // re-emits them as real ReadDirectoryChangesW notifications.
    let sink = drive.event_sink();
    fs.start_event_pump(move |batch| sink.push(batch)).await?;
    tracing::info!(mountpoint, "mounted; press Ctrl-C to unmount");
    tokio::signal::ctrl_c().await?;
    tracing::info!("unmounting");
    drive.unmount();
    fs.shutdown(); // persist the auto-cache manifest
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Config {
        serde_yaml::from_str(text).expect("should load")
    }

    /// Two mounts and a client-level default, which is the shape the name
    /// lookup has to cope with: `work` inherits the exclude it never states.
    const CONFIG: &str = "client:\n  exclude: [node_modules]\n  mounts:\n\
         \x20   docs: { url: 'tcp://h/docs', at: 'D:' }\n\
         \x20   work: { url: 'ssh://azure/projects', at: 'P:' }\n";

    #[test]
    fn a_configured_name_arrives_with_the_client_defaults_folded_in() {
        let mount = resolve_named(&parse(CONFIG), "work").expect("work is configured");
        assert_eq!(mount.url, "ssh://azure/projects");
        assert_eq!(mount.at, PathBuf::from("P:"));
        assert_eq!(mount.exclude, ["node_modules"]);
    }

    /// The reason the two forms are told apart by positional COUNT: a name
    /// that is not in the file has to fail as a name, listing what is, rather
    /// than being handed to the url parser and failing as a hostname.
    #[test]
    fn an_unknown_name_lists_the_configured_ones() {
        let err = resolve_named(&parse(CONFIG), "wrok")
            .expect_err("wrok is not configured")
            .to_string();
        assert!(err.contains("wrok"), "{err}");
        assert!(err.contains("docs") && err.contains("work"), "{err}");
    }

    /// A config with no mounts at all cannot list alternatives, so it says
    /// where mounts go instead of printing an empty list.
    #[test]
    fn an_unknown_name_with_nothing_configured_says_where_mounts_go() {
        let err = resolve_named(&parse("version: 3\n"), "work")
            .expect_err("nothing is configured")
            .to_string();
        assert!(err.contains("work"), "{err}");
        assert!(err.contains("client.mounts"), "{err}");
    }

    #[test]
    fn a_size_flag_beats_the_configured_size() {
        let configured = Some(SizeField::Human("2M".into()));
        assert_eq!(
            resolve_size(Some("8M".into()), configured.clone(), "auto_cache_max").unwrap(),
            Some("8M".to_string())
        );
    }

    /// Both spellings of a configured size reach `run` as the same number.
    #[test]
    fn a_configured_size_travels_as_bytes() {
        let two_meg = (2 * 1024 * 1024).to_string();
        for configured in [SizeField::Human("2M".into()), SizeField::Bytes(2 * 1024 * 1024)] {
            assert_eq!(
                resolve_size(None, Some(configured), "auto_cache_max").unwrap(),
                Some(two_meg.clone())
            );
        }
        assert_eq!(resolve_size(None, None, "auto_cache_max").unwrap(), None);
    }

    #[test]
    fn an_unparseable_configured_size_names_the_key() {
        let err = resolve_size(None, Some(SizeField::Human("2X".into())), "auto_cache_budget")
            .expect_err("2X is not a size")
            .to_string();
        assert!(err.contains("auto_cache_budget"), "{err}");
    }
}
