use std::path::PathBuf;

use crate::config::{app_dir, cache_root};
use crate::urls::{export_key, host_key, parse_url, require_export};

/// Delete downloaded blobs. Safe by construction: the cache tree contains
/// nothing but re-downloadable data, and the overlay — files that exist on
/// no server — lives under `data/`, which this never touches.
pub fn clear(target: Option<String>, all: bool, data_dir: Option<PathBuf>) -> anyhow::Result<()> {
    let root = match &data_dir {
        Some(r) => r.join("cache"),
        None => app_dir().join("cache"),
    };

    if all {
        if root.is_dir() {
            std::fs::remove_dir_all(&root)?;
        }
        println!("cleared every cached blob under {}", root.display());
        return Ok(());
    }

    let url = target.ok_or_else(|| anyhow::anyhow!("give a mount url, or --all"))?;
    let (_, export) = parse_url(&url)?;
    let export = require_export(export, &url)?;
    let host = host_key(&url);
    let key = export_key(&export);

    let dir = match &data_dir {
        Some(r) => r.join("cache").join(&host),
        None => cache_root(&host),
    };
    let blobs = dir.join(&key);
    let manifest = dir.join(format!("{key}.manifest.json"));

    let mut removed = false;
    if blobs.is_dir() {
        std::fs::remove_dir_all(&blobs)?;
        removed = true;
    }
    if manifest.is_file() {
        std::fs::remove_file(&manifest)?;
        removed = true;
    }
    if removed {
        println!("cleared the cache for {host}/{key}");
    } else {
        println!("nothing cached for {host}/{key}");
    }
    Ok(())
}
