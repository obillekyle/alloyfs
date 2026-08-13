//! Agent-only path resolution primitive. Everything else that used to live
//! here (attr synthesis, positional I/O, errno mapping) moved to `ds-common`.

use std::path::{Path, PathBuf};

use ds_common::io_to_code;
use ds_proto::{ErrorCode, RelPath};

/// Resolve `rel` inside `root_canon`, refusing anything that escapes it.
/// Symlinks are resolved server-side, so a link pointing outside the export
/// is caught by the prefix check.
///
/// UNCHECKED against export excludes — which is why request handlers must
/// never call this directly: `Export::resolve` / `Export::resolve_new` are
/// the security choke points that add exclusion enforcement on top.
pub(crate) fn resolve_unchecked(root_canon: &Path, rel: &RelPath) -> Result<PathBuf, ErrorCode> {
    rel.validate()?;
    if rel.is_root() {
        return Ok(root_canon.to_path_buf());
    }
    let mut full = root_canon.to_path_buf();
    for comp in rel.0.split('/') {
        full.push(comp);
    }
    let canon = std::fs::canonicalize(&full).map_err(|e| io_to_code(&e))?;
    if !canon.starts_with(root_canon) {
        tracing::warn!(path = %rel, "path escapes export root (symlink?)");
        return Err(ErrorCode::PermissionDenied);
    }
    Ok(canon)
}
