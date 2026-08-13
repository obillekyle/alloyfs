use std::path::PathBuf;
use std::sync::Arc;

use crate::config::{default_data_dir, parse_size, MountConfig};
use crate::urls::{connect_target, dialer_for, mount_key, require_export, whoami};

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
    token: Option<String>,
) -> anyhow::Result<()> {
    // File values first, CLI flags override. Sizes stay None when neither
    // set one, so a server suggestion (proto v2+) can fill them in.
    let file_cfg = match &config {
        Some(path) => MountConfig::load(path)?,
        None => MountConfig::default(),
    };
    let excludes = if excludes.is_empty() {
        file_cfg.exclude
    } else {
        excludes
    };
    let pins = if pins.is_empty() { file_cfg.pin } else { pins };
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
    let no_server_defaults = no_server_defaults || file_cfg.no_server_defaults;
    let token = token.or(file_cfg.token);
    let data_dir = data_dir.or(file_cfg.data_dir).unwrap_or_else(default_data_dir);

    let (conn, export) = connect_target(&url, &remote_cmd, &whoami(), token.as_deref()).await?;
    let export = require_export(export, &url)?;
    tracing::info!(server = conn.server_name, proto = conn.proto, "connected");
    let opts = ds_client::ClientOptions {
        mount_key: mount_key(&url, &export),
        excludes,
        pins,
        auto_cache_max,
        auto_cache_budget,
        // CLI mounts default to auto-caching (2M files, 512M budget) when
        // neither the user nor the server picked values.
        auto_cache_max_fallback: 2 * 1024 * 1024,
        auto_cache_budget_fallback: 512 * 1024 * 1024,
        no_server_defaults,
        // Survive connection loss: re-dial, re-attach, re-open handles,
        // resubscribe events. Locks do not survive (documented).
        dialer: Some(dialer_for(&url, &remote_cmd, &whoami(), token)),
        data_dir,
    };
    let fs = ds_client::RemoteFs::attach_with(conn, &export, opts).await?;
    // Each platform starts the event pump itself, wiring server events into
    // its native notification mechanism.
    mount_platform(fs, mountpoint, &export).await
}

#[cfg(unix)]
async fn mount_platform(
    fs: Arc<ds_client::RemoteFs>,
    mountpoint: PathBuf,
    export: &str,
) -> anyhow::Result<()> {
    let export = export.to_string();
    let (notifier_tx, notifier_rx) = tokio::sync::oneshot::channel();
    let mount_fs = fs.clone();
    // The mount blocks its thread until unmount; the runtime stays free to
    // service the connection and the event pump.
    let mount_task = tokio::task::spawn_blocking(move || {
        ds_mount_fuse::mount(mount_fs, &mountpoint, &export, move |notifier| {
            let _ = notifier_tx.send(notifier);
        })
    });
    if let Ok(notifier) = notifier_rx.await {
        // Server events → cache invalidation (inside the pump) → kernel cache
        // eviction, so the next stat/read/listing is instantly fresh.
        let pump_fs = fs.clone();
        fs.start_event_pump(move |batch| {
            ds_mount_fuse::apply_events_native(&pump_fs, &notifier, batch);
        })
        .await?;
    }
    mount_task.await??;
    fs.shutdown(); // persist the auto-cache manifest
    Ok(())
}

#[cfg(windows)]
async fn mount_platform(
    fs: Arc<ds_client::RemoteFs>,
    mountpoint: PathBuf,
    export: &str,
) -> anyhow::Result<()> {
    let mountpoint = mountpoint.to_string_lossy().into_owned();
    // mount() returns once the WinFsp dispatcher is running; its threads call
    // back into RemoteFs, which block_on's this runtime — so we keep the
    // runtime alive here until Ctrl-C, then unmount cleanly.
    let drive = ds_mount_winfsp::mount(fs.clone(), &mountpoint, export)?;
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
