use std::path::PathBuf;

use crate::config::default_data_dir;
use crate::urls::{mount_key, parse_url, require_export};

/// `cache clear`: delete a mount's blobs + manifest (SAFE while unmounted).
/// The overlay (local-only excluded files) is NEVER touched — deleting that
/// would lose the only copy.
pub fn clear(target: Option<String>, all: bool, data_dir: Option<PathBuf>) -> anyhow::Result<()> {
    let base = data_dir.unwrap_or_else(default_data_dir).join("cache");
    if all {
        if base.exists() {
            std::fs::remove_dir_all(&base)?;
        }
        println!("cleared all caches under {}", base.display());
        return Ok(());
    }
    let url = target.ok_or_else(|| anyhow::anyhow!("pass a mount url (with export) or --all"))?;
    let (_, export) = parse_url(&url)?;
    let export = require_export(export, &url)?;
    let key = mount_key(&url, &export);
    let dir = base.join(&key);
    let manifest = base.join(format!("{key}.manifest.json"));
    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }
    let _ = std::fs::remove_file(&manifest);
    println!("cleared cache for {key}");
    Ok(())
}
