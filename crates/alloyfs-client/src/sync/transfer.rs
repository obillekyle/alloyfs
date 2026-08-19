//! File transfers for the sync engine.
//!
//! The mtime-mirroring invariant lives here: after every transfer both sides
//! carry the same (size, mtime). Downloads stamp the remote mtime onto the
//! staged file BEFORE the atomic rename; uploads Setattr the local mtime
//! onto the server after the last write. That equality is what makes
//! "clean" cheaply decidable ever after (reconcile short-circuits, echo
//! suppression compares exactly, crash windows heal as adopts).

use std::path::Path;

use alloyfs_proto::{Attr, OpenFlags, RelPath, Request, Response, DATA_CHUNK};
use alloyfs_transport::MuxConnection;

use crate::FsError;

/// Download `rel` into `staging` (a .ds-tmp file beside the destination),
/// stamping the remote mtime. Returns the attr the content corresponds to.
/// 8 chunks in flight — the walker's proven number.
pub(crate) async fn download_to(conn: &MuxConnection, rel: &str, staging: &Path) -> Result<Attr, FsError> {
    let flags = OpenFlags {
        read: true,
        ..OpenFlags::default()
    };
    let (fh, attr) = match conn
        .request(Request::Open {
            path: RelPath(rel.to_string()),
            flags,
        })
        .await??
    {
        Response::Opened { fh, attr } => (fh, attr),
        _ => return Err(alloyfs_proto::ErrorCode::Io.into()),
    };
    let result = fetch_body(conn, fh, attr.size, staging).await;
    let _ = conn.request(Request::Release { fh }).await;
    result?;
    let f = std::fs::OpenOptions::new()
        .write(true)
        .open(staging)
        .map_err(|_| alloyfs_proto::ErrorCode::Io)?;
    f.set_modified(attr.mtime)
        .map_err(|_| alloyfs_proto::ErrorCode::Io)?;
    Ok(attr)
}

async fn fetch_body(conn: &MuxConnection, fh: u64, size: u64, staging: &Path) -> Result<(), FsError> {
    use futures::StreamExt;
    // Bounded like the walker's: `size` is whatever the server reported, and
    // pre-allocating it outright turns a corrupt or hostile value into an
    // abort rather than an error.
    const MAX_PREALLOC: u64 = 8 * 1024 * 1024;
    let mut out = Vec::with_capacity(size.min(MAX_PREALLOC) as usize);
    let chunks: Vec<(u64, u32)> = (0..size.max(1))
        .step_by(DATA_CHUNK as usize)
        .map(|off| (off, ((size - off.min(size)).min(DATA_CHUNK as u64)) as u32))
        .filter(|&(_, len)| len > 0)
        .collect();
    let mut stream =
        futures::stream::iter(chunks.into_iter().map(|(offset, len)| async move {
            (offset, conn.request(Request::Read { fh, offset, len }).await)
        }))
        .buffered(8); // in-order: chunks append directly
    while let Some((_, resp)) = stream.next().await {
        match resp?? {
            Response::Data(data) => out.extend_from_slice(&data),
            _ => return Err(alloyfs_proto::ErrorCode::Io.into()),
        }
    }
    drop(stream);
    // The chunk list was computed from the size at open; a truncate while the
    // reads were in flight makes the later ones short and the file INCOMPLETE.
    // Without this the short result is written out, stamped with the remote
    // mtime by the caller, and recorded in the manifest under the ORIGINAL
    // size — so the next reconcile compares a full-size manifest entry against
    // a short local file and sees nothing to do. The walker has always checked
    // this (`walker.rs`); the sync path never did.
    if out.len() as u64 != size {
        tracing::debug!(
            expected = size,
            got = out.len(),
            "file changed mid-fetch; leaving it for the next event"
        );
        return Err(alloyfs_proto::ErrorCode::Io.into());
    }
    std::fs::write(staging, &out).map_err(|_| alloyfs_proto::ErrorCode::Io)?;
    Ok(())
}

/// Upload the local file at `full` to remote `rel` (create or overwrite),
/// then mirror its mtime server-side. Returns the server's final Attr —
/// the version to record in the manifest.
pub(crate) async fn upload(conn: &MuxConnection, rel: &str, full: &Path) -> Result<Attr, FsError> {
    // A LOCAL read, and its failure is reported as `ErrorCode::Io` — which
    // prints as "server I/O error" and has sent at least one investigation to
    // the wrong machine. The usual cause is benign and local: the file was
    // renamed or removed between the watcher noticing it and this running, so
    // there is nothing at `full` any more. Log what actually happened before
    // flattening it into a wire code.
    let data = std::fs::read(full).map_err(|e| {
        tracing::debug!(path = %full.display(), error = %e, "local read failed before upload");
        alloyfs_proto::ErrorCode::Io
    })?;
    let mtime = std::fs::metadata(full)
        .and_then(|m| m.modified())
        .map_err(|_| alloyfs_proto::ErrorCode::Io)?;
    let flags = OpenFlags {
        read: false,
        write: true,
        truncate: true,
        ..OpenFlags::default()
    };
    let fh = match conn
        .request(Request::Create {
            path: RelPath(rel.to_string()),
            flags,
            mode: 0o644,
        })
        .await??
    {
        Response::Opened { fh, .. } => fh,
        _ => return Err(alloyfs_proto::ErrorCode::Io.into()),
    };
    let mut result: Result<(), FsError> = Ok(());
    for (i, chunk) in data.chunks(DATA_CHUNK as usize).enumerate() {
        let resp = conn
            .request(Request::Write {
                fh,
                offset: (i * DATA_CHUNK as usize) as u64,
                data: bytes::Bytes::copy_from_slice(chunk),
                expect_version: None, // LWW by design; conflicts were pre-checked
            })
            .await;
        match resp {
            // v5+ servers answer with the attributes attached; the uploader
            // reads neither field, only "it landed".
            Ok(Ok(Response::Written { .. } | Response::WrittenAttr { .. })) => {}
            _ => {
                result = Err(alloyfs_proto::ErrorCode::Io.into());
                break;
            }
        }
    }
    let _ = conn.request(Request::Release { fh }).await;
    result?;
    // Mirror the local mtime; the response Attr carries the final version.
    match conn
        .request(Request::Setattr {
            path: RelPath(rel.to_string()),
            size: None,
            mtime: Some(mtime),
            mode: None,
        })
        .await??
    {
        Response::Attr(attr) => Ok(attr),
        _ => Err(alloyfs_proto::ErrorCode::Io.into()),
    }
}

/// `<path>.sync-conflict-<utc timestamp>` beside the original — the loser of
/// a conflict lives here, and syncs to the other side like any file.
pub(crate) fn conflict_name(path: &str) -> String {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{path}.sync-conflict-{ts}")
}
