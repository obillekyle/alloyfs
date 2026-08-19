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

use alloyfs_proto::{OpenFlags, RelPath, Request, Response, DATA_CHUNK};
use tokio::sync::{mpsc, Semaphore};

use crate::autocache::{stage_write, AutoCache};
use crate::remote_fs::RemoteFs;

const FILE_CONCURRENCY: usize = 4;
/// Directories listed concurrently per BFS level. Discovery is pure metadata
/// — small frames, no disk pressure server-side — so it tolerates far more
/// parallelism than content fetches do.
const DIR_CONCURRENCY: usize = 16;
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
        // BFS by LEVELS, every directory of a level listed concurrently. The
        // old walk awaited one Readdir at a time, which priced discovery at
        // one round trip per directory — 535 directories measured 35.8 s, and
        // nearly all of it was the network sitting idle between questions.
        // Depth is what bounds it now: a level of W directories costs
        // ceil(W / DIR_CONCURRENCY) round trips instead of W.
        let mut level: Vec<RelPath> = vec![RelPath(String::new())];
        let mut tasks = tokio::task::JoinSet::new();
        let mut ok = 0usize;

        while !level.is_empty() {
            walked_dirs += level.len();
            let mut next_level = Vec::new();
            for window in level.chunks(DIR_CONCURRENCY) {
                let listings = futures::future::join_all(window.iter().map(|dir| {
                    let fs = fs.clone();
                    let dir = dir.clone();
                    async move {
                        // Follow this one directory's cursor pages serially —
                        // pages of one listing are dependent; directories are
                        // not, and they are the parallelism that matters.
                        let mut all = Vec::new();
                        let mut cursor = 0u64;
                        loop {
                            let resp = fs
                                .conn()
                                .request(Request::Readdir {
                                    path: dir.clone(),
                                    cursor,
                                })
                                .await;
                            match resp {
                                Ok(Ok(Response::Dir { entries, next_cursor })) => {
                                    all.extend(entries);
                                    match next_cursor {
                                        Some(c) => cursor = c,
                                        None => break,
                                    }
                                }
                                _ => break, // transient: skip dir, keep walking
                            }
                        }
                        (dir, all)
                    }
                }))
                .await;

                for (dir, entries) in listings {
                    for e in entries {
                        let child = dir.join(&e.name);
                        if fs.is_overlay(&child) {
                            continue; // overlay paths are already local
                        }
                        match e.attr.kind {
                            alloyfs_proto::FileKind::Dir => next_level.push(child),
                            alloyfs_proto::FileKind::File => {
                                if cache.wants(&child, e.attr.size) && cache.needs_fetch(&child, &e.attr) {
                                    // The permit is taken INSIDE the task, so a
                                    // full fetch pipeline never stalls the walk
                                    // — discovery and download overlap instead
                                    // of strangling each other.
                                    let sem = sem.clone();
                                    let fs = fs.clone();
                                    let cache = cache.clone();
                                    fetched += 1;
                                    tasks.spawn(async move {
                                        let _permit = sem.acquire_owned().await.unwrap();
                                        fetch_one(&fs, &cache, &child).await
                                    });
                                }
                            }
                            alloyfs_proto::FileKind::Symlink => {}
                        }
                    }
                }
                // Reap finished fetches as we go, so the set never grows to
                // "every file in the export" on a big tree.
                while let Some(done) = tasks.try_join_next() {
                    if matches!(done, Ok(true)) {
                        ok += 1;
                    }
                }
            }
            level = next_level;
        }
        while let Some(done) = tasks.join_next().await {
            if matches!(done, Ok(true)) {
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
        if attr.kind != alloyfs_proto::FileKind::File
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
    // Fire-and-forget, for the reason `RemoteFs::release` gives: the reply was
    // discarded anyway, and awaiting it held one of the FILE_CONCURRENCY slots
    // for a full round trip per file. That wait was one of the three RTTs every
    // small file cost.
    let _ = fs.conn().send_oneway(Request::Release { fh }).await;
    result
}
