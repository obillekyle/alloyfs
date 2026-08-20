//! Attach-time machinery: mount-defaults negotiation, overlay and auto-cache
//! construction, the background tasks, and the reconnect supervisor.
//!
//! Everything here runs once per mount (or, for the supervisor, once per
//! connection death) — none of it is on an I/O path. `RemoteFs::attach_with`
//! is the only caller of the `pub(super)` builders.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use alloyfs_proto::{ErrorCode, RelPath, Request, Response};
use alloyfs_transport::MuxConnection;

use crate::autocache::AutoCache;
use crate::error::FsError;
use crate::metacache::MetaCache;
use crate::options::ClientOptions;
use crate::overlay::Overlay;

use super::lock_ranges::HeldRange;
use super::{RemoteFs, NO_SERVER_FH};

/// A mount's settled configuration: the merged options, plus the two cache
/// sizes with their fallbacks already resolved.
pub(super) struct Negotiated {
    pub(super) opts: ClientOptions,
    pub(super) auto_cache_max: u64,
    pub(super) auto_cache_budget: u64,
}

/// Fold the server's suggested client settings (protocol v2+) in underneath
/// the client's own.
///
/// Precedence is `explicit client value > server suggestion > fallback`, but
/// what "underneath" means differs per setting:
///
/// * Lists (`excludes`, `pins`) are UNIONED, the client's own entries first.
///   Both sides' patterns name something somebody wanted excluded or pinned,
///   so letting either side replace the other loses a stated intent. This is
///   deliberately unlike the config file's client-defaults merge, where a
///   per-mount list REPLACES the global one — there, both lists were written
///   by the same person, and replacing is how they say "not that, this".
/// * Sizes apply only where the client made no explicit choice, which is why
///   `auto_cache_max: Some(0)` is an OFF switch a server cannot override
///   while `None` is an invitation for it to suggest one.
/// * `no_server_defaults` skips the exchange entirely: `ask` is never
///   invoked, so nothing is sent. Same for a v1 peer, which cannot decode the
///   request variant at all.
///
/// `ask` yielding `None` — a server that refused, a connection that dropped —
/// leaves the client's own configuration exactly as it was.
pub(super) async fn negotiate_defaults<F, Fut>(mut opts: ClientOptions, proto: u16, ask: F) -> Negotiated
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Option<Response>>,
{
    if !opts.no_server_defaults && proto >= 2 {
        if let Some(Response::MountDefaults {
            exclude,
            pin,
            auto_cache_max,
            auto_cache_budget,
        }) = ask().await
        {
            let suggested = exclude.len() + pin.len();
            for e in exclude {
                if !opts.excludes.contains(&e) {
                    opts.excludes.push(e);
                }
            }
            for p in pin {
                if !opts.pins.contains(&p) {
                    opts.pins.push(p);
                }
            }
            if opts.auto_cache_max.is_none() {
                opts.auto_cache_max = auto_cache_max;
            }
            if opts.auto_cache_budget.is_none() {
                opts.auto_cache_budget = auto_cache_budget;
            }
            if suggested > 0 || auto_cache_max.is_some() || auto_cache_budget.is_some() {
                tracing::info!(
                    excludes = opts.excludes.len(),
                    pins = opts.pins.len(),
                    "applied server-suggested mount defaults (--no-server-defaults to opt out)"
                );
            }
        }
    }
    Negotiated {
        auto_cache_max: opts.auto_cache_max.unwrap_or(opts.auto_cache_max_fallback),
        auto_cache_budget: opts.auto_cache_budget.unwrap_or(opts.auto_cache_budget_fallback),
        opts,
    }
}

/// The local overlay backing the `--exclude`d paths — built on EVERY mount,
/// user excludes or not.
///
/// It used to be `None` when the user excluded nothing, and that gate quietly
/// disabled the built-in `LOCAL_ARTIFACTS` routing: `Overlay::new` compiles
/// those defaults into its matcher, but a mount with no `--exclude` never
/// built one, so `desktop.ini`, `Thumbs.db` and `.DS_Store` went to the wire
/// — a full round trip per probe, and Explorer probes them in every directory
/// it shows. Measured: 0.6 ms with the overlay against 62 ms without.
///
/// Worse than the latency: those names are server-excluded by default too, so
/// WRITING them (Explorer customising a folder) got `NotFound` back. With the
/// overlay always present they land locally and work.
///
/// The always-on overlay costs nothing until used — `Overlay::new` no longer
/// touches the disk, and the directory materialises on the first write of an
/// excluded path.
pub(super) fn build_overlay(opts: &ClientOptions) -> Result<Option<Overlay>, FsError> {
    let root = opts.data_dir.join("overlay").join(&opts.mount_key);
    let ov = Overlay::new(root, &opts.excludes).map_err(|e| {
        tracing::error!(error = %e, "overlay init failed");
        ErrorCode::Io
    })?;
    let orphans = ov.orphans();
    if !orphans.is_empty() {
        tracing::warn!(
            ?orphans,
            "overlay contains entries that no longer match any --exclude \
             pattern; they are invisible until the pattern returns"
        );
    }
    Ok(Some(ov))
}

/// Paths the auto-cache has decided to download, drained by the walker.
pub(super) type FetchQueue = tokio::sync::mpsc::UnboundedReceiver<RelPath>;

/// What `build_auto_cache` hands back: the cache, the walker's fetch queue,
/// and the metadata snapshot — all `None` together when caching is off.
pub(super) type CachePieces = (Option<Arc<AutoCache>>, Option<FetchQueue>, Option<MetaCache>);

/// The auto-download cache, the walker's fetch queue, and the metadata
/// snapshot. All are `None` unless something asks for a cache: a nonzero size
/// limit, or a pin (which must be held locally whatever the limit says).
pub(super) fn build_auto_cache(
    opts: &ClientOptions,
    auto_cache_max: u64,
    auto_cache_budget: u64,
) -> Result<CachePieces, FsError> {
    if auto_cache_max == 0 && opts.pins.is_empty() {
        return Ok((None, None, None));
    }
    let root = opts.cache_dir.join(&opts.mount_key);
    let manifest = opts.cache_dir.join(format!("{}.manifest.json", opts.mount_key));
    // The metadata snapshot lives beside the manifest — same directory, same
    // mount key — because it lives under the same trust root: the tree token
    // the manifest records is the only thing that can prove its listings.
    let meta = MetaCache::new(opts.cache_dir.join(format!("{}.metadata.json", opts.mount_key)));
    let (cache, rx) = AutoCache::load(crate::autocache::AutoCacheConfig {
        max_file_size: auto_cache_max,
        budget: auto_cache_budget.max(1),
        pins: opts.pins.clone(),
        root,
        manifest,
    })
    .map_err(|e| {
        tracing::error!(error = %e, "auto-cache init failed");
        ErrorCode::Io
    })?;
    Ok((Some(Arc::new(cache)), Some(rx), Some(meta)))
}

/// Start the long-lived tasks, each only when its feature is configured: the
/// cache walker with its manifest flusher, and the reconnect supervisor.
pub(super) fn spawn_background_tasks(fs: &Arc<RemoteFs>, fetch_rx: Option<FetchQueue>) {
    if let (Some(cache), Some(rx)) = (fs.cache.clone(), fetch_rx) {
        crate::walker::spawn(fs.clone(), cache.clone(), rx);
        // Manifest flusher: every 30 s when dirty.
        let flusher = cache;
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(30));
            loop {
                tick.tick().await;
                let c = flusher.clone();
                let _ = tokio::task::spawn_blocking(move || c.flush_manifest()).await;
            }
        });
    }
    if fs.dialer.is_some() {
        tokio::spawn(reconnect_supervisor(fs.clone()));
    }
    // The batcher's age flusher: whatever the queue holds when a tick fires
    // goes out, which bounds the ack-early window to one FLUSH_AGE whatever
    // the workload does. Weak, so an unmounted filesystem can actually die;
    // spawn_blocking because flushing drives the synchronous request path.
    if fs.batch.is_some() {
        let weak = Arc::downgrade(fs);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(crate::batcher::FLUSH_AGE);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let Some(fs) = weak.upgrade() else { break };
                if fs.batch.as_ref().is_some_and(|b| !b.is_empty()) {
                    let _ = tokio::task::spawn_blocking(move || fs.flush_batch()).await;
                }
            }
        });
    }
}

/// Reconnect supervisor: when the connection dies, dial a replacement with
/// exponential backoff, re-attach, re-open every live handle on the new
/// session, replay each handle's advisory lock, then swap it in and bump
/// the epoch (which re-triggers the event pump's subscription).
///
/// Lock replay is best-effort by nature: the server freed the old session's
/// locks the moment it saw the disconnect, so a waiting contender may
/// legally win the lock before our replay arrives. When that happens the
/// handle is POISONED (subsequent I/O fails EIO) rather than silently
/// continuing without mutual exclusion — apps handle EIO; none can handle a
/// lock that quietly stopped existing. Even a successful replay means "held
/// again", not "held continuously" — during an asymmetric partition the old
/// session's locks persist up to the ~30 s lease, and between its release
/// and our replay another client may have held and released the lock.
async fn reconnect_supervisor(fs: Arc<RemoteFs>) {
    let dialer = fs.dialer.clone().expect("supervisor spawned only with a dialer");
    loop {
        fs.conn().closed().await;
        tracing::warn!("connection lost; reconnecting");
        let mut delay = Duration::from_millis(500);
        let new_conn = loop {
            match dialer().await {
                Ok(c) => break c,
                Err(e) => {
                    tracing::warn!(error = %e, retry_in = ?delay, "reconnect failed");
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(Duration::from_secs(15));
                }
            }
        };
        // Re-attach before swapping so concurrent ops keep failing fast on
        // the dead conn instead of racing a half-initialized one.
        match new_conn
            .request(Request::Attach {
                export: fs.export.clone(),
            })
            .await
        {
            Ok(Ok(Response::AttachOk { .. })) => {}
            other => {
                tracing::error!(?other, "re-attach failed; retrying");
                continue;
            }
        }
        // Re-open live handles on the new session; kernel fhs stay stable,
        // only the server_fh translation changes. A handle's lock replays
        // right after its re-open (it needs the fresh server_fh); all of it
        // happens BEFORE the conn swap so lock state is settled by the time
        // the new connection is observable.
        let mut reopened = 0usize;
        let mut failed = 0usize;
        let mut locks_restored = 0usize;
        let mut locks_lost = 0usize;
        for entry in fs.open_files.iter() {
            let state = entry.value();
            state.ra.clear(); // in-flight blocks belong to the dead conn
            *state.blob.write().unwrap() = None;
            // A handle the old session never knew about has nothing to restore.
            // It cannot hold a lock either — taking one materialises it — so
            // there is no mutual exclusion to have lost. It re-opens on its own
            // if a read ever escapes the cache.
            if state.server_fh.load(Ordering::Acquire) == NO_SERVER_FH {
                continue;
            }
            let held: Vec<HeldRange> = state.lock.lock().unwrap().clone();
            let req = Request::Open {
                path: state.path.clone(),
                flags: state.flags,
            };
            match new_conn.request(req).await {
                Ok(Ok(Response::Opened { fh, .. })) => {
                    state.server_fh.store(fh, Ordering::Release);
                    reopened += 1;
                    // EVERY range, or the handle is poisoned. Restoring some
                    // of what was held and reporting success would leave the
                    // application believing in exclusion it no longer has,
                    // which is the one outcome this path exists to prevent.
                    if !held.is_empty() {
                        let proto = new_conn.proto;
                        let mut all = true;
                        for h in &held {
                            if !replay_lock(&new_conn, fh, h, proto).await {
                                all = false;
                                break;
                            }
                        }
                        if all {
                            locks_restored += 1;
                        } else {
                            locks_lost += 1;
                            state.poisoned.store(true, Ordering::Release);
                            tracing::warn!(
                                path = %state.path,
                                ranges = held.len(),
                                "lock lost across reconnect; handle poisoned (EIO until reopened)"
                            );
                        }
                    }
                }
                _ => {
                    // File may be gone; subsequent ops get BadHandle → EBADF —
                    // unless it held a lock, which makes the loss a mutual-
                    // exclusion break: poison so it surfaces as EIO.
                    failed += 1;
                    if !held.is_empty() {
                        locks_lost += 1;
                        state.poisoned.store(true, Ordering::Release);
                    }
                }
            }
        }
        *fs.conn.write().unwrap() = new_conn;
        // The server may have restarted (versions reset) or missed changes:
        // trust nothing cached until revalidated.
        fs.invalidate_all();
        if let Some(cache) = &fs.cache {
            cache.mark_all_unverified();
        }
        fs.conn_epoch.send_modify(|e| *e += 1);
        tracing::info!(reopened, failed, locks_restored, locks_lost, "reconnected");
    }
}

/// Re-acquire one advisory lock on the new session. Retries WouldBlock a few
/// times: the server may not have processed the OLD session's disconnect yet,
/// so the first attempts can lose to our own zombie's still-held lock.
///
/// Replays through whichever shape the NEW connection negotiated. A reconnect
/// can land on a different server, so a range taken at v7 may have to be
/// replayed against a v6 peer — where the honest thing is to coarsen it,
/// claiming more than was held rather than less.
async fn replay_lock(conn: &Arc<MuxConnection>, fh: u64, held: &HeldRange, proto: u16) -> bool {
    let req = if proto >= 7 {
        Request::LockRange {
            fh,
            owner: held.owner,
            kind: held.kind,
            start: held.start,
            len: held.wire_len(),
            wait: false,
        }
    } else {
        Request::Lock {
            fh,
            kind: held.kind,
            wait: false,
        }
    };
    for attempt in 0..4 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        match conn.request(req.clone()).await {
            Ok(Ok(Response::Ok)) => return true,
            Ok(Err(ErrorCode::WouldBlock)) => continue, // maybe our zombie; retry
            _ => return false,
        }
    }
    false
}

/// Mount-defaults negotiation. Every rule here decides what a mount is
/// actually configured with, and each one has a plausible-looking wrong
/// answer — a suggestion that overrides an explicit `0`, a list that replaces
/// instead of unions, an opt-out that still sends the request — so they are
/// pinned individually rather than left to an end-to-end mount test.
#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn suggests(
        exclude: &[&str],
        pin: &[&str],
        auto_cache_max: Option<u64>,
        auto_cache_budget: Option<u64>,
    ) -> Response {
        Response::MountDefaults {
            exclude: exclude.iter().map(|s| (*s).to_string()).collect(),
            pin: pin.iter().map(|s| (*s).to_string()).collect(),
            auto_cache_max,
            auto_cache_budget,
        }
    }

    /// Negotiate against a v2 server that answers with `resp`.
    async fn against(opts: ClientOptions, resp: Response) -> Negotiated {
        negotiate_defaults(opts, 2, move || async move { Some(resp) }).await
    }

    /// An explicit client value wins, including the explicit OFF: `Some(0)`
    /// means "no auto-cache on this mount" and an eager server must not talk
    /// the client back into one.
    #[tokio::test]
    async fn explicit_client_values_beat_the_server_suggestion() {
        let opts = ClientOptions {
            auto_cache_max: Some(0),
            auto_cache_budget: Some(4096),
            auto_cache_max_fallback: 111,
            auto_cache_budget_fallback: 222,
            ..ClientOptions::default()
        };
        let out = against(opts, suggests(&[], &[], Some(9_999_999), Some(8_888_888))).await;
        assert_eq!(out.auto_cache_max, 0);
        assert_eq!(out.auto_cache_budget, 4096);
    }

    /// No explicit choice: the server's suggestion outranks the fallback.
    #[tokio::test]
    async fn the_server_suggestion_beats_the_fallback() {
        let opts = ClientOptions {
            auto_cache_max_fallback: 111,
            auto_cache_budget_fallback: 222,
            ..ClientOptions::default()
        };
        let out = against(opts, suggests(&[], &[], Some(4096), Some(8192))).await;
        assert_eq!(out.auto_cache_max, 4096);
        assert_eq!(out.auto_cache_budget, 8192);
    }

    /// Neither side chose: the fallback applies. Suggesting one size and not
    /// the other must not drag the unsuggested one along.
    #[tokio::test]
    async fn the_fallback_applies_where_neither_side_chose() {
        let opts = ClientOptions {
            auto_cache_max_fallback: 111,
            auto_cache_budget_fallback: 222,
            ..ClientOptions::default()
        };
        let out = against(opts, suggests(&[], &[], Some(4096), None)).await;
        assert_eq!(out.auto_cache_max, 4096);
        assert_eq!(out.auto_cache_budget, 222);
    }

    /// Lists are UNIONED, not replaced, with the client's own entries first —
    /// a server suggestion never costs the client a pattern it asked for, and
    /// never silently drops one the server asked for either. Duplicates
    /// collapse rather than accumulating on every mount.
    #[tokio::test]
    async fn lists_are_unioned_with_the_clients_entries_first() {
        let opts = ClientOptions {
            excludes: vec!["mine/**".into(), "shared/**".into()],
            pins: vec!["*.mine".into()],
            ..ClientOptions::default()
        };
        let out = against(
            opts,
            suggests(&["shared/**", "theirs/**"], &["*.mine", "*.theirs"], None, None),
        )
        .await;
        assert_eq!(out.opts.excludes, ["mine/**", "shared/**", "theirs/**"]);
        assert_eq!(out.opts.pins, ["*.mine", "*.theirs"]);
    }

    /// `no_server_defaults` opts out of ASKING, not merely of applying the
    /// answer: the request must never be sent.
    #[tokio::test]
    async fn no_server_defaults_skips_the_exchange_entirely() {
        let asked = Cell::new(false);
        let opts = ClientOptions {
            excludes: vec!["mine/**".into()],
            no_server_defaults: true,
            auto_cache_max_fallback: 111,
            ..ClientOptions::default()
        };
        let out = negotiate_defaults(opts, 2, || async {
            asked.set(true);
            Some(suggests(&["theirs/**"], &[], Some(9_999_999), None))
        })
        .await;
        assert!(!asked.get(), "the request must not be sent at all");
        assert_eq!(out.opts.excludes, ["mine/**"]);
        assert_eq!(out.auto_cache_max, 111);
    }

    /// A v1 peer cannot decode the request variant, so it is never sent one.
    #[tokio::test]
    async fn a_v1_server_is_never_asked() {
        let asked = Cell::new(false);
        let out = negotiate_defaults(ClientOptions::default(), 1, || async {
            asked.set(true);
            Some(suggests(&["theirs/**"], &[], Some(9_999_999), None))
        })
        .await;
        assert!(!asked.get(), "the request must not be sent at all");
        assert!(out.opts.excludes.is_empty());
    }

    /// A refused or dropped exchange is not a configuration change: the
    /// client keeps its own values and its own fallbacks.
    #[tokio::test]
    async fn an_unanswered_exchange_leaves_the_client_configuration_alone() {
        let opts = ClientOptions {
            excludes: vec!["mine/**".into()],
            auto_cache_max_fallback: 111,
            auto_cache_budget_fallback: 222,
            ..ClientOptions::default()
        };
        let out = negotiate_defaults(opts, 2, || async { None }).await;
        assert_eq!(out.opts.excludes, ["mine/**"]);
        assert_eq!(out.auto_cache_max, 111);
        assert_eq!(out.auto_cache_budget, 222);
    }
}
