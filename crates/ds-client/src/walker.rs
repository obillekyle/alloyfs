//! The eager auto-cache walker and the re-fetch queue drainer.
//!
//! Walks the export once per mount (async, straight over the connection —
//! bypassing the sync facade keeps the InodeTable and attr cache free of
//! thousands of entries the mount may never touch), then services re-fetch
//! requests (from events and our own writes) forever.
//!
//! Concurrency: 4 files in flight × up to 8 concurrent 128 KiB chunks each —
//! ~32 outstanding requests. Cross-FILE parallelism is what makes small-file
//! prefetch fast on high-RTT links; per-read chunk parallelism alone can't.

use std::sync::Arc;

use ds_proto::{OpenFlags, RelPath, Request, Response, DATA_CHUNK};
use tokio::sync::{mpsc, Semaphore};

use crate::autocache::{stage_write, AutoCache};
use crate::remote_fs::RemoteFs;

const FILE_CONCURRENCY: usize = 4;
const CHUNK_CONCURRENCY: usize = 8;

pub(crate) fn spawn(
    fs: Arc<RemoteFs>,
    cache: Arc<AutoCache>,
    mut fetch_rx: mpsc::UnboundedReceiver<RelPath>,
) {
    tokio::spawn(async move {
        let started = std::time::Instant::now();
        let sem = Arc::new(Semaphore::new(FILE_CONCURRENCY));
        let mut walked_dirs = 0usize;
        let mut fetched = 0usize;
        let mut queue: std::collections::VecDeque<RelPath> = [RelPath(String::new())].into();
        let mut tasks = Vec::new();

        while let Some(dir) = queue.pop_front() {
            walked_dirs += 1;
            let mut cursor = 0u64;
            loop {
                let resp = fs
                    .conn()
                    .request(Request::Readdir {
                        path: dir.clone(),
                        cursor,
                    })
                    .await;
                let (entries, next) = match resp {
                    Ok(Ok(Response::Dir { entries, next_cursor })) => (entries, next_cursor),
                    _ => break, // transient failure: skip this dir, keep walking
                };
                for e in entries {
                    let child = dir.join(&e.name);
                    if fs.is_overlay(&child) {
                        continue; // overlay paths are already local
                    }
                    match e.attr.kind {
                        ds_proto::FileKind::Dir => queue.push_back(child),
                        ds_proto::FileKind::File => {
                            if cache.wants(&child, e.attr.size) && cache.needs_fetch(&child, &e.attr) {
                                let permit = sem.clone().acquire_owned().await.unwrap();
                                let fs = fs.clone();
                                let cache = cache.clone();
                                fetched += 1;
                                tasks.push(tokio::spawn(async move {
                                    let ok = fetch_one(&fs, &cache, &child).await;
                                    drop(permit);
                                    ok
                                }));
                            }
                        }
                        ds_proto::FileKind::Symlink => {}
                    }
                }
                match next {
                    Some(c) => cursor = c,
                    None => break,
                }
            }
        }
        let mut ok = 0usize;
        for t in tasks {
            if matches!(t.await, Ok(true)) {
                ok += 1;
            }
        }
        let (entries, bytes) = cache.stats();
        tracing::info!(
            walked_dirs,
            queued = fetched,
            fetched = ok,
            cached_entries = entries,
            cached_bytes = bytes,
            elapsed_s = started.elapsed().as_secs_f32(),
            "auto-cache walk complete"
        );
        let c = cache.clone();
        let _ = tokio::task::spawn_blocking(move || c.flush_manifest()).await;

        // Re-fetch queue: events and local writes land here forever after.
        while let Some(path) = fetch_rx.recv().await {
            if fs.is_overlay(&path) {
                continue;
            }
            let permit = sem.clone().acquire_owned().await.unwrap();
            let fs = fs.clone();
            let cache = cache.clone();
            tokio::spawn(async move {
                let _ = fetch_one(&fs, &cache, &path).await;
                drop(permit);
            });
        }
    });
}

/// Fetch one file into the cache: open → verify it (still) qualifies →
/// chunked concurrent read → stage `.part` → rename into place → commit.
async fn fetch_one(fs: &Arc<RemoteFs>, cache: &Arc<AutoCache>, path: &RelPath) -> bool {
    let flags = OpenFlags {
        read: true,
        ..OpenFlags::default()
    };
    let opened = fs
        .conn()
        .request(Request::Open {
            path: path.clone(),
            flags,
        })
        .await;
    let (fh, attr) = match opened {
        Ok(Ok(Response::Opened { fh, attr })) => (fh, attr),
        _ => return false, // vanished (or excluded server-side): fine
    };
    let result = async {
        if attr.kind != ds_proto::FileKind::File
            || !cache.wants(path, attr.size)
            || !cache.needs_fetch(path, &attr)
        {
            return false;
        }
        // Chunk list, fetched CHUNK_CONCURRENCY at a time in order windows.
        let mut data = Vec::with_capacity(attr.size as usize);
        let mut pos = 0u64;
        while pos < attr.size {
            let mut batch = Vec::new();
            for _ in 0..CHUNK_CONCURRENCY {
                if pos >= attr.size {
                    break;
                }
                let want = ((attr.size - pos) as u32).min(DATA_CHUNK);
                batch.push((pos, want));
                pos += want as u64;
            }
            let conn = fs.conn();
            let responses = futures::future::join_all(batch.iter().map(|&(off, len)| {
                let conn = conn.clone();
                async move { conn.request(Request::Read { fh, offset: off, len }).await }
            }))
            .await;
            for resp in responses {
                match resp {
                    Ok(Ok(Response::Data(chunk))) => data.extend_from_slice(&chunk),
                    _ => return false,
                }
            }
        }
        if data.len() as u64 != attr.size {
            return false; // changed mid-fetch; an event will requeue it
        }
        let stage = cache.blob_stage_path(path);
        let final_path = cache.blob_final_path(path);
        let commit_path = path.clone();
        let cache2 = cache.clone();
        let pinned = cache.pin_match(path);
        tokio::task::spawn_blocking(move || {
            if stage_write(&stage, &data).is_err() {
                return false;
            }
            if std::fs::rename(&stage, &final_path).is_err() {
                let _ = std::fs::remove_file(&stage);
                return false;
            }
            cache2.commit(&commit_path, &attr, pinned);
            true
        })
        .await
        .unwrap_or(false)
    }
    .await;
    let _ = fs.conn().request(Request::Release { fh }).await;
    result
}
