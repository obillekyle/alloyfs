//! The eager auto-cache walker and the re-fetch queue drainer.
//!
//! Walks the export once per mount (async, straight over the connection —
//! bypassing the sync facade keeps the InodeTable and attr cache free of
//! thousands of entries the mount may never touch), then services re-fetch
//! requests (from events and our own writes) forever.
//!
//! A completed tree pass is also snapshotted as METADATA (metacache.rs): the
//! per-directory listings, tagged with the tree token they were read at. The
//! token-match skip path installs that snapshot into the warm tier, so a
//! remount of an unchanged export answers its first navigation locally
//! instead of paying one round trip per directory.
//!
//! Concurrency: 4 files in flight × up to 8 concurrent 128 KiB chunks each —
//! ~32 outstanding requests. Cross-FILE parallelism is what makes small-file
//! prefetch fast on high-RTT links; per-read chunk parallelism alone can't.

use std::sync::Arc;

use alloyfs_proto::{Attr, ManyEntry, OpenFlags, RelPath, Request, Response, DATA_CHUNK};
use tokio::sync::{mpsc, Semaphore};

use crate::autocache::{stage_write, AutoCache};
use crate::remote_fs::RemoteFs;

const FILE_CONCURRENCY: usize = 4;
/// Directories listed concurrently per BFS level. Discovery is pure metadata
/// — small frames, no disk pressure server-side — so it tolerates far more
/// parallelism than content fetches do.
const DIR_CONCURRENCY: usize = 16;
const CHUNK_CONCURRENCY: usize = 8;

/// Bytes asked of one `ReadMany`. The agent clamps this to what a frame can
/// hold; asking for its ceiling means a full batch per round trip.
const READ_MANY_BUDGET: u32 = 768 * 1024;

/// Files batched into one `ReadMany` before it is sent. Bounded by size as
/// well as count — the point is to fill a frame, and a batch of large files
/// fills one long before 256 of them accumulate.
const BATCH_FILES: usize = 256;

pub(crate) fn spawn(fs: Arc<RemoteFs>, cache: Arc<AutoCache>, fetch_rx: mpsc::UnboundedReceiver<RelPath>) {
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
        // Files discovered but not yet asked for, and the bytes they add up
        // to. Accumulating them is what turns one request per file into one
        // request per frame.
        let mut pending: Vec<RelPath> = Vec::new();
        let mut pending_bytes = 0u64;

        // Before anything else: has the export changed at all since this
        // cache was last complete?
        //
        // One `TreeToken` answers that in a single exchange, and a match means
        // every blob on disk is still current — no tree to fetch, no files to
        // compare, nothing to walk. This is the difference between a warm
        // remount costing one round trip and costing a full discovery pass.
        //
        // The token is usable for this precisely because it is derived from
        // the export's CONTENT rather than from a session counter: an
        // `ssh://` agent is spawned fresh per mount, so anything session-shaped
        // (`seq`) restarts and cannot speak for a cache written by an earlier
        // mount. See the `Manifest` field comment.
        let saved_token = cache.saved_tree_token();
        // Attach2 (v9+) already carried the token; spend it instead of
        // asking again — take-once, so any LATER check in this task's
        // lifetime goes back to the wire rather than trusting a value that
        // only described the export as of attach. Taken up front because BOTH
        // paths below want it: the skip check, and the metadata salvage on
        // the walking path.
        let attach_token = fs.attach_tree_token.swap(0, std::sync::atomic::Ordering::AcqRel);
        // The export's token as of now, learned as cheaply as possible; 0
        // while unknown. The skip check fills it from the wire when attach
        // did not carry one.
        let mut current_token = attach_token;
        if saved_token != 0 {
            // Captured BEFORE the token is asked for: the snapshot install
            // below must not land listings from before an invalidation that
            // happened while the answer was in flight — the same in-flight
            // re-install bug the dir_epoch guard closed for readdir.
            let warm_epoch = fs.warm_epoch_now();
            let current = if current_token != 0 {
                Ok(Ok(Response::TreeToken { token: current_token }))
            } else if fs.conn().proto >= 6 {
                fs.conn().request(Request::TreeToken).await
            } else {
                // A saved token with a pre-v6 server on the wire: the cache
                // was written by a newer session than the one now attached
                // (reconnects can land elsewhere; caches outlive servers).
                // The agent cannot answer TreeToken — and must not be ASKED,
                // because an unknown variant index kills the connection
                // rather than failing cleanly. Answer for it: cannot verify,
                // so walk.
                Ok(Err(alloyfs_proto::ErrorCode::VersionMismatch))
            };
            if let Ok(Ok(Response::TreeToken { token })) = &current {
                current_token = *token;
            }
            match current {
                Ok(Ok(Response::TreeToken { token })) if token == saved_token => {
                    cache.note_walk_skipped();
                    // The same proof that lets the blobs serve without
                    // re-validation lets the saved LISTINGS serve: an
                    // unchanged token means the export is still what the
                    // metadata snapshot describes, so the first navigation
                    // is answered locally instead of one round trip per
                    // directory.
                    let warm_fs = fs.clone();
                    let warmed =
                        tokio::task::spawn_blocking(move || warm_fs.adopt_meta_snapshot(token, warm_epoch))
                            .await
                            .unwrap_or(0);
                    tracing::info!(
                        token,
                        entries = cache.stats().0,
                        warmed_dirs = warmed,
                        elapsed_s = started.elapsed().as_secs_f32(),
                        "auto-cache is current for this export; skipping the walk"
                    );
                    // Straight to serving the re-fetch queue: events and our
                    // own writes still keep the cache honest from here.
                    drain_refetch_queue(fs, cache, fetch_rx, sem).await;
                    return;
                }
                // Changed, unindexed (token 0), or an agent that cannot
                // answer: fall through and do the work.
                _ => {}
            }
        }

        // Discovery is happening — but the metadata snapshot may still be
        // salvageable. The manifest token (blob completeness) and the
        // snapshot token (metadata completeness) are separate claims: the
        // loader zeroes the MANIFEST token when a blob is missing or the
        // budget evicts, and that is a statement about bytes, not listings.
        // If the export's current token still equals the snapshot's, the
        // listings are as token-proven as they ever were — adopt them, serve
        // the first navigation warm, and let the walk below refill only the
        // blobs. `adopt_meta_snapshot` discards on any mismatch, so the one
        // call is both the salvage and the cleanup; when no current token is
        // in hand (a pre-v9 agent with no saved manifest token), the snapshot
        // is deleted unproven, exactly as before.
        {
            let stale = fs.clone();
            let token = current_token;
            let salvaged = tokio::task::spawn_blocking(move || {
                if token != 0 {
                    let epoch = stale.warm_epoch_now();
                    let warmed = stale.adopt_meta_snapshot(token, epoch);
                    if warmed == 0 {
                        stale.discard_meta_snapshot();
                    }
                    warmed
                } else {
                    stale.discard_meta_snapshot();
                    0
                }
            })
            .await
            .unwrap_or(0);
            if salvaged > 0 {
                tracing::info!(
                    warmed_dirs = salvaged,
                    "metadata adopted despite incomplete blobs; walking to refill content"
                );
            }
        }

        // Captured BEFORE the discovery whose entries become the snapshot:
        // the walk-end install's guard must span the whole period the
        // snapshot's CONTENT describes, not the milliseconds between save
        // and install. It was captured at install time, and a mkdir landing
        // mid-walk (bun init's first act, during the ~30 s first-mount walk)
        // bumped the epoch BEFORE the too-late capture — so the guard
        // blessed an install of pre-mkdir listings, and the warm tier then
        // answered a hard NotFound for a directory this mount had just
        // created. bun reported ENOENT for package.json; the healing only
        // came from unrelated busts.
        let content_epoch = fs.warm_epoch_now();

        // One exchange instead of one per directory, where the agent can. The
        // BFS below stays as the fallback and is still what runs against a
        // pre-v6 agent or an unindexed export.
        let indexed = discover_by_tree(&fs).await;
        // Kept only when the tree served the whole discovery: a BFS walk has
        // no token to speak for, and recording one from a fallback pass would
        // be claiming knowledge the pass never had.
        let complete_at_token = if indexed.is_some() {
            current_tree_token(&fs).await
        } else {
            0
        };
        let mut meta_snapshot = None;
        if let Some((entries, pages_token)) = indexed {
            // The metadata snapshot is built from the SAME entries the fetch
            // pass consumes, but only trusted when the token did not move
            // between the tree pages and the end-of-pass check — the
            // no-stitching rule discover_by_tree applies between its own
            // pages, extended one hop. A moved token means these listings
            // describe a state the export has already left, and tagging them
            // with the newer token would hand the next mount proven-looking
            // listings of the wrong tree.
            if pages_token == complete_at_token {
                meta_snapshot = Some(crate::metacache::snapshot_from_tree(&entries, complete_at_token));
            }
            walked_dirs = entries
                .iter()
                .filter(|(_, a)| a.kind == alloyfs_proto::FileKind::Dir)
                .count();
            for (path, attr) in entries {
                if attr.kind != alloyfs_proto::FileKind::File || fs.is_overlay(&path) {
                    continue;
                }
                if !cache.wants(&path, attr.size) || !cache.needs_fetch(&path, &attr) {
                    continue;
                }
                fetched += 1;
                pending_bytes += attr.size;
                pending.push(path);
                if pending.len() >= BATCH_FILES || pending_bytes >= READ_MANY_BUDGET as u64 {
                    spawn_fetch_batch(&mut tasks, &sem, &fs, &cache, std::mem::take(&mut pending));
                    pending_bytes = 0;
                }
                while let Some(done) = tasks.try_join_next() {
                    ok += done.unwrap_or(0);
                }
            }
            // Nothing left to walk: the tree already named every path.
            level.clear();
        }

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
                                    fetched += 1;
                                    pending_bytes += e.attr.size;
                                    pending.push(child);
                                    // Send once a frame's worth has gathered.
                                    // Batching by BOTH count and bytes matters:
                                    // 256 tiny files fill no frame, and a
                                    // handful of large ones overfill it.
                                    if pending.len() >= BATCH_FILES
                                        || pending_bytes >= READ_MANY_BUDGET as u64
                                    {
                                        spawn_fetch_batch(
                                            &mut tasks,
                                            &sem,
                                            &fs,
                                            &cache,
                                            std::mem::take(&mut pending),
                                        );
                                        pending_bytes = 0;
                                    }
                                }
                            }
                            alloyfs_proto::FileKind::Symlink => {}
                        }
                    }
                }
                // Reap finished fetches as we go, so the set never grows to
                // "every file in the export" on a big tree.
                while let Some(done) = tasks.try_join_next() {
                    ok += done.unwrap_or(0);
                }
            }
            level = next_level;
        }
        // Whatever never reached a full batch still has to be fetched.
        if !pending.is_empty() {
            spawn_fetch_batch(&mut tasks, &sem, &fs, &cache, pending);
        }
        while let Some(done) = tasks.join_next().await {
            ok += done.unwrap_or(0);
        }
        // Record the token ONLY if the pass actually finished the job: the
        // tree served discovery, and everything it queued was fetched. A token
        // beside an incomplete cache is worse than no token, because the next
        // mount would trust it and skip the walk that would have finished the
        // work — the token would be telling the truth about the export while
        // saying nothing about the cache.
        let complete = complete_at_token != 0 && ok == fetched;
        if complete {
            cache.record_tree_token(complete_at_token);
            // The snapshot lands BEFORE the manifest flush below. The
            // manifest's token is what lets the next mount skip its walk, so
            // it must never reach disk ahead of the snapshot it would let
            // that mount adopt. The reverse order is merely cold (skip, no
            // listings to install); this order is warm as well.
            if let Some(snap) = meta_snapshot.take() {
                let snap_fs = fs.clone();
                let token = complete_at_token;
                let warmed = tokio::task::spawn_blocking(move || {
                    snap_fs.save_meta_snapshot(&snap);
                    // The FIRST mount deserves the warm tier too, not only the
                    // remount that skips: the listings were just proven at
                    // this token, and without this the mount that did all the
                    // work navigates cold while the next one navigates warm.
                    // The guard epoch is the one captured BEFORE discovery —
                    // any local mutation during the walk bumps it and the
                    // install refuses, same guard, same safe direction (a
                    // skipped install costs a cold lookup; a stale one
                    // answers hard NotFound for a file that exists, which is
                    // exactly what it did to bun init's fresh directory).
                    snap_fs.adopt_meta_snapshot(token, content_epoch)
                })
                .await
                .unwrap_or(0);
                tracing::debug!(warmed_dirs = warmed, "fresh snapshot adopted for this mount");
            }
        }
        let (entries, bytes) = cache.stats();
        tracing::info!(
            walked_dirs,
            queued = fetched,
            fetched = ok,
            cached_entries = entries,
            cached_bytes = bytes,
            tree_token = complete_at_token,
            complete,
            elapsed_s = started.elapsed().as_secs_f32(),
            "auto-cache walk complete"
        );
        let c = cache.clone();
        let _ = tokio::task::spawn_blocking(move || c.flush_manifest()).await;

        // Re-fetch queue: events and local writes land here forever after.
        drain_refetch_queue(fs, cache, fetch_rx, sem).await;
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
        // `attr.size` is whatever the SERVER said it was. Pre-allocating it
        // outright turns a corrupt or hostile value into an abort — capacity
        // overflow, or the OOM killer — rather than an error, and a pinned
        // path bypasses the size ceiling that would otherwise bound it. Ask
        // for a sane amount and let the Vec grow if the file really is large.
        //
        // This bounds the ALLOCATION, not the buffering: a genuinely huge
        // pinned file is still assembled whole in memory. Streaming it to the
        // cache file instead is the real fix and is its own change.
        const MAX_PREALLOC: u64 = 8 * 1024 * 1024;
        let mut data = Vec::with_capacity(attr.size.min(MAX_PREALLOC) as usize);
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

/// Stage a fetched body and commit it to the cache. Shared by the one-file and
/// the bulk paths, which differ only in how the bytes arrived.
async fn commit_blob(
    cache: &Arc<AutoCache>,
    path: &RelPath,
    attr: &alloyfs_proto::Attr,
    data: Vec<u8>,
) -> bool {
    let stage = cache.blob_stage_path(path);
    let final_path = cache.blob_final_path(path);
    let commit_path = path.clone();
    let cache2 = cache.clone();
    let pinned = cache.pin_match(path);
    let attr = *attr;
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

/// Fetch a batch of files in one exchange and commit each. Returns how many
/// landed.
///
/// This is where the walk stops being latency-bound. Per file the old path
/// costs an Open, one or more Reads and a Release; measured over 200 small
/// files that was 179 ms each against ~4 ms of actual transfer. One `ReadMany`
/// carries as many whole files as fit in a frame.
///
/// The reply is always a PREFIX of the request — the server stops when its
/// budget is spent — so what is outstanding is simply what was not answered,
/// and the loop re-asks for it. Pre-v8 agents are gated up front (they cannot
/// decode the variant, so asking kills the connection instead of failing);
/// two fallbacks then keep this from ever losing a file: any failed request
/// reverts to the per-file path, and so does a single file the server
/// declines as too large for a bulk reply, which is exactly the case the
/// chunked reader exists for.
async fn fetch_many(fs: &Arc<RemoteFs>, cache: &Arc<AutoCache>, batch: Vec<RelPath>) -> usize {
    let mut ok = 0usize;
    // Checked BEFORE sending, per call because a reconnect can land on a
    // different server. The fallback arm below cannot cover the pre-v8 case:
    // an agent that lacks the variant does not answer with an error, its
    // postcard decode fails at the codec and it KILLS THE CONNECTION —
    // measured against a v6 agent, where every mount looped walk → ReadMany →
    // reset → reconnect and the cache never filled. Same rule as
    // `require_proto`: too-old is something to know, never to discover by
    // sending.
    if fs.conn().proto < 8 {
        for path in &batch {
            if fetch_one(fs, cache, path).await {
                ok += 1;
            }
        }
        return ok;
    }
    let mut remaining = batch;

    while !remaining.is_empty() {
        // The clone here is structural, not an oversight: `ReadMany` owns
        // its paths (it crosses a wire), and the list is still needed
        // afterwards — to zip against a reply that is only a PREFIX, to
        // fall back per-file if the request fails, and to keep the
        // unserved tail for the next round. Moving it into the request
        // would mean recovering it from the request, which is strictly
        // worse. It costs one path list per round, at mount time only.
        let resp = fs
            .conn()
            .request(Request::ReadMany {
                paths: remaining.clone(),
                budget: READ_MANY_BUDGET,
            })
            .await;
        let entries = match resp {
            Ok(Ok(Response::Many(entries))) if !entries.is_empty() => entries,
            // A refusal, a dead connection, or a reply that served nothing —
            // which would spin here. Read them the ordinary way instead of
            // dropping them on the floor.
            _ => {
                for path in &remaining {
                    if fetch_one(fs, cache, path).await {
                        ok += 1;
                    }
                }
                return ok;
            }
        };
        let served = entries.len().min(remaining.len());
        for (path, entry) in remaining.iter().zip(entries) {
            match entry {
                ManyEntry::File { attr, data } => {
                    if cache.wants(path, attr.size) && commit_blob(cache, path, &attr, data.into()).await {
                        ok += 1;
                    }
                }
                ManyEntry::Skipped(alloyfs_proto::ErrorCode::TooLarge) => {
                    if fetch_one(fs, cache, path).await {
                        ok += 1;
                    }
                }
                // Gone, excluded server-side, or not a regular file. All fine:
                // an event requeues anything that comes back.
                ManyEntry::Skipped(_) => {}
            }
        }
        remaining.drain(..served);
    }
    ok
}

/// Every file in the export, and what it costs to find them.
///
/// v6+ agents index the export and serve the whole subtree in one paginated
/// exchange, so discovery is a handful of round trips regardless of shape. The
/// BFS this replaces cost one `Readdir` per DIRECTORY — 535 of them measured
/// 35.8 s against a 60 ms link, nearly all of it the network idle between
/// questions.
///
/// `None` means the tree was not available and the caller should walk: an
/// agent older than v6, or an export the server declined to index (too large
/// for its cap, or indexing failed), which it signals with token 0.
///
/// Returns the (nonzero) token the pages carried alongside the entries, so
/// the caller can tell whether a token it fetches LATER still describes this
/// same tree — the metadata snapshot hangs on that comparison.
async fn discover_by_tree(fs: &Arc<RemoteFs>) -> Option<(Vec<(RelPath, Attr)>, u64)> {
    if fs.conn().proto < 6 {
        return None;
    }
    let mut out = Vec::new();
    let mut cursor = None;
    let mut token_seen = 0u64;
    let mut requests = 0usize;
    loop {
        let resp = fs
            .conn()
            .request(Request::Tree {
                path: RelPath(String::new()),
                cursor,
            })
            .await;
        let (entries, next_cursor, token) = match resp {
            Ok(Ok(Response::Tree {
                entries,
                next_cursor,
                token,
            })) => (entries, next_cursor, token),
            // Unsupported, or the request failed: walk instead.
            _ => return None,
        };
        requests += 1;
        if token == 0 {
            return None; // not indexed — the server is telling us to walk
        }
        // A token that moves between pages means the export changed underneath
        // the read. Stitching two states into one tree would be worse than not
        // answering, and the walk is a correct answer.
        if token_seen != 0 && token != token_seen {
            tracing::debug!("export changed while reading its tree; falling back to a walk");
            return None;
        }
        token_seen = token;
        out.extend(entries.into_iter().map(|e| (e.path, e.attr)));
        match next_cursor {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }
    tracing::debug!(requests, entries = out.len(), "export tree fetched");
    Some((out, token_seen))
}

/// The export's current tree token, or 0 when there is not one to have —
/// a pre-v6 agent, an unindexed export, or a request that failed.
///
/// Asked separately from the tree itself rather than reusing the token the
/// `Tree` pages carried: this is taken AFTER discovery finished, so it
/// describes the export as of the end of the pass. A token from the start
/// would claim the cache is current as of a moment before the fetches ran,
/// and anything changed in between would be invisible to the next mount.
async fn current_tree_token(fs: &Arc<RemoteFs>) -> u64 {
    if fs.conn().proto < 6 {
        return 0;
    }
    match fs.conn().request(Request::TreeToken).await {
        Ok(Ok(Response::TreeToken { token })) => token,
        _ => 0,
    }
}

/// Serve the re-fetch queue until the mount closes: events and our own writes
/// enqueue paths here forever, whichever way the initial pass ended. One copy,
/// because the skip path and the completed walk both used to carry it inline
/// and identical — the kind of duplication that stays identical only until an
/// edit lands in one of them.
async fn drain_refetch_queue(
    fs: Arc<RemoteFs>,
    cache: Arc<AutoCache>,
    mut fetch_rx: mpsc::UnboundedReceiver<RelPath>,
    sem: Arc<Semaphore>,
) {
    while let Some(path) = fetch_rx.recv().await {
        if fs.is_overlay(&path) {
            cache.clear_demand(&path);
            continue;
        }
        let permit = sem.clone().acquire_owned().await.unwrap();
        let fs = fs.clone();
        let cache = cache.clone();
        tokio::spawn(async move {
            let _ = fetch_one(&fs, &cache, &path).await;
            // Settled either way. Leaving the marker would keep `wants`
            // saying yes for a path nothing is fetching, and would stop a
            // later read from re-demanding after a failure.
            cache.clear_demand(&path);
            drop(permit);
        });
    }
}
/// Queue one accumulated batch for fetching, permit taken inside the task so a
/// full pipeline never stalls discovery. One copy for the three places a
/// batch leaves the accumulator: the tree path's threshold, the BFS path's
/// threshold, and the end-of-walk remainder.
fn spawn_fetch_batch(
    tasks: &mut tokio::task::JoinSet<usize>,
    sem: &Arc<Semaphore>,
    fs: &Arc<RemoteFs>,
    cache: &Arc<AutoCache>,
    batch: Vec<RelPath>,
) {
    let sem = sem.clone();
    let fs = fs.clone();
    let cache = cache.clone();
    tasks.spawn(async move {
        let _permit = sem.acquire_owned().await.unwrap();
        fetch_many(&fs, &cache, batch).await
    });
}
