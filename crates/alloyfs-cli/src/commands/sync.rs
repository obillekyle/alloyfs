//! `alloyfs sync <url>/<export> <dir>` — bidirectional real-directory
//! sync. The reason this exists: watchers on the LOCAL directory get real,
//! full-fidelity file events for remote changes (a mounted filesystem can
//! never deliver those on Linux).

use std::path::PathBuf;

use crate::config::data_root;
use crate::urls::{connect_target, dialer_for, export_key, host_key, require_export, whoami};

#[allow(clippy::too_many_arguments)]
pub async fn run(
    url: String,
    dir: PathBuf,
    remote_cmd: String,
    excludes: Vec<String>,
    debounce_ms: u64,
    conflict_policy: String,
    data_dir: Option<PathBuf>,
    token: Option<String>,
    one_shot: bool,
) -> anyhow::Result<()> {
    let policy: alloyfs_client::ConflictPolicy =
        conflict_policy.parse().map_err(|e: String| anyhow::anyhow!(e))?;
    let (conn, export) = connect_target(&url, &remote_cmd, &whoami(), token.as_deref()).await?;
    let export = require_export(export, &url)?;
    tracing::info!(server = conn.server_name, proto = conn.proto, "connected");

    // ~/.alloyfs/data/<host>/ — sync keeps only durable state (baselines).
    let host = host_key(&url);
    let data_dir = match data_dir {
        Some(root) => root.join("data").join(&host),
        None => data_root(&host),
    };
    // Same export synced into two different local dirs must not share state.
    let dir_tag = {
        let canon = std::fs::canonicalize(&dir)
            .unwrap_or_else(|_| dir.clone())
            .to_string_lossy()
            .to_lowercase();
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for b in canon.bytes() {
            hash ^= b as u64;
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        format!("{:08x}", hash as u32)
    };
    let opts = alloyfs_client::SyncOptions {
        excludes,
        data_dir,
        sync_key: format!("{}-{dir_tag}", export_key(&export)),
        debounce: std::time::Duration::from_millis(debounce_ms),
        conflict_policy: policy,
        dialer: Some(dialer_for(&url, &remote_cmd, &whoami(), token)),
        one_shot,
    };
    let engine = alloyfs_client::SyncEngine::start(conn, &export, &dir, opts).await?;

    if one_shot {
        // Drain the initial reconcile, then exit.
        while !engine.is_quiescent() {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        engine.shutdown();
        return Ok(());
    }
    tracing::info!(dir = %dir.display(), "syncing; press Ctrl-C to stop");
    tokio::signal::ctrl_c().await?;
    tracing::info!("stopping");
    engine.shutdown();
    Ok(())
}
