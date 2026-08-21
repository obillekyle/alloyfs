//! The metadata caches and their invalidation: the TTL'd attr and listing
//! caches, the token-proven warm tier, the on-disk metadata snapshot, and
//! the per-handle read-cache busting.
//!
//! Everything here answers one question two ways: "can this metadata still be
//! served without asking the server?" The TTL pair answers it by time, scaled
//! to the event pump's health; the warm tier answers it by proof, valid until
//! any mutation or trust break clears it.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use alloyfs_proto::{Attr, RelPath};

use crate::metacache::MetaSnapshot;

use super::RemoteFs;

/// Metadata TTLs: a short floor for every state where the event pump cannot
/// be trusted, and NO timer at all while it can.
///
/// Event-driven invalidation is the real freshness mechanism — remote changes
/// bust attrs, listings, readahead and the warm tier the moment the pump
/// applies them, and our own writes bust synchronously. Under a healthy pump
/// a timer adds nothing but cost, and the cost was measured: the old 60 s
/// ceiling expired listings MID-WALK on the bench's second warm round, so an
/// identical walk measured 19 ms or 92 ms depending on where the clock stood
/// — WAN refreshes for answers the event stream had already promised to
/// correct. The warm tier has served under exactly this no-timer contract
/// since it existed; the live caches now share it.
///
/// The choice is made at READ time from the pump's health right now, not
/// stamped into the entry at insert. That gives the snap-down its teeth: the
/// moment health drops, an entry cached an hour ago under the armed regime
/// fails the 5-second test immediately — there is no window where old entries
/// coast on the trust that existed when they were written. (Lag and resync
/// also flush the caches outright; the read-time check is what covers the
/// states that do not.)
const TTL_ARMED: Duration = Duration::MAX;
const ATTR_TTL_POLL: Duration = Duration::from_secs(5);

/// Listing floor, kept separate from the attr floor because the two are
/// tunable independently — a listing going stale mis-sorts a directory, an
/// attribute going stale mis-serves a file, and those are not the same
/// severity.
const DIR_TTL_POLL: Duration = Duration::from_secs(5);

impl RemoteFs {
    pub(crate) fn cache_attr(&self, ino: u64, attr: Attr) {
        self.attr_cache.insert(ino, (attr, Instant::now()));
    }

    /// Reports whether an entry was actually dropped — the event pump's
    /// bulk re-warm only asks about attrs someone demonstrably had warm.
    /// Every call bumps `attr_epoch`, dropped or not: an in-flight re-warm
    /// reply compares the epoch before seeding, and "any event happened"
    /// is the cheap, safe reason to discard it (same global trade as
    /// `dir_epoch`).
    pub fn invalidate_attr(&self, ino: u64) -> bool {
        self.attr_epoch.fetch_add(1, std::sync::atomic::Ordering::Release);
        self.attr_cache.remove(&ino).is_some()
    }

    pub fn invalidate_all(&self) {
        self.attr_cache.clear();
        self.attr_epoch.fetch_add(1, Ordering::Release);
        self.dir_cache.clear();
        self.dir_epoch.fetch_add(1, Ordering::Release);
        // Every caller of this is a "we may have missed events" path — lag,
        // resync, reconnect — and missed events are the one thing the warm
        // tier has no TTL to grow out of. It goes entirely, and it stays
        // gone: nothing re-installs it until the next mount re-proves the
        // token.
        self.warm.clear();
        self.warm_epoch.fetch_add(1, Ordering::Release);
    }

    /// Drop the cached listing of `path`'s PARENT. Every mutation that adds,
    /// removes or reshapes an entry calls this with the child's path — the
    /// server strips self-origin events, so our own changes will never arrive
    /// through the pump and the busting has to be synchronous, right here.
    pub(crate) fn invalidate_parent_dir(&self, path: &RelPath) {
        if let Some((parent, _)) = path.split() {
            // The warm tier is path-keyed, so it must not wait on an ino:
            // a warm listing can exist for a directory this session has
            // never allocated a number for.
            self.bust_warm(&parent);
            if let Some(pino) = self.ino.ino_of(&parent) {
                self.dir_cache.remove(&pino);
                self.dir_epoch.fetch_add(1, Ordering::Release);
            }
        }
    }

    /// Drop one directory's warm listing, and note the mutation so a
    /// snapshot install in flight cannot land listings from before it.
    pub(crate) fn bust_warm(&self, dir: &RelPath) {
        self.warm.remove(dir);
        self.warm_epoch.fetch_add(1, Ordering::Release);
    }

    /// Drop the warm listings of `root` and everything under it. For renames:
    /// the warm tier is path-keyed, so a moved directory's own listing (and
    /// its descendants') sit under keys that no longer name anything. The
    /// ino-keyed `dir_cache` keeps a renamed directory's listing — its number
    /// is stable and the names inside did not move — but re-keying warm to
    /// chase that would buy little for the bookkeeping; one cold readdir
    /// after a rename is the cheaper trade.
    pub(crate) fn warm_forget_subtree(&self, root: &RelPath) {
        self.warm.remove(root);
        let prefix = format!("{}/", root.0);
        self.warm.retain(|k, _| !k.0.starts_with(&prefix));
        self.warm_epoch.fetch_add(1, Ordering::Release);
    }

    /// How many directories the warm tier currently answers for. Zero on any
    /// mount that did not adopt a snapshot; what the tests (and a curious
    /// operator) use to tell "walk skipped and metadata warm" from "walk
    /// skipped, metadata cold".
    pub fn warm_dirs(&self) -> usize {
        self.warm.len()
    }

    /// Is the event pump subscribed and keeping up right now? Diagnostic
    /// counterpart of `auto_cache_walk_skipped`: "which TTL regime is this
    /// mount in" is otherwise invisible from outside.
    pub fn event_pump_healthy(&self) -> bool {
        self.pump_healthy.load(Ordering::Acquire)
    }

    /// The attr TTL as of this instant — untimed under a healthy pump, 5 s
    /// otherwise. Read-time selection is deliberate; see the TTL constants.
    pub(super) fn attr_ttl(&self) -> Duration {
        if self.pump_healthy.load(Ordering::Acquire) {
            TTL_ARMED
        } else {
            ATTR_TTL_POLL
        }
    }

    /// The listing TTL as of this instant. Same switch as `attr_ttl`.
    pub(super) fn dir_ttl(&self) -> Duration {
        if self.pump_healthy.load(Ordering::Acquire) {
            TTL_ARMED
        } else {
            DIR_TTL_POLL
        }
    }

    /// The warm-invalidation epoch right now. The walker captures it BEFORE
    /// asking the server for its tree token, for the same reason `readdir`
    /// captures `dir_epoch` before its first page: the install that follows
    /// a match must not land listings from before an invalidation that
    /// happened while the answer was in flight.
    pub(crate) fn warm_epoch_now(&self) -> u64 {
        self.warm_epoch.load(Ordering::Acquire)
    }

    /// Install the on-disk metadata snapshot into the warm tier, when it
    /// proves out: right token, and nothing invalidated a listing since
    /// `epoch_at_check` was captured. Returns how many directories landed.
    pub(crate) fn adopt_meta_snapshot(&self, token: u64, epoch_at_check: u64) -> usize {
        let Some(meta) = &self.meta else { return 0 };
        let Some(snap) = meta.load_for(token) else {
            return 0;
        };
        // Same rule as readdir's insert guard, same direction of error: a
        // mutation observed since the capture means these listings may
        // describe the past, and a skipped install costs a cold first
        // navigation while a stale one answers hard NotFound for a file
        // that exists.
        if self.warm_epoch.load(Ordering::Acquire) != epoch_at_check {
            tracing::debug!("a mutation raced the snapshot install; serving cold instead");
            return 0;
        }
        let n = snap.dirs.len();
        for (dir, listing) in snap.dirs {
            self.warm.insert(RelPath(dir), listing);
        }
        n
    }

    /// Persist a fresh metadata snapshot (walker, on a completed pass).
    pub(crate) fn save_meta_snapshot(&self, snap: &MetaSnapshot) {
        if let Some(meta) = &self.meta {
            meta.save(snap);
        }
    }

    /// Delete the metadata snapshot: it is no longer proven for whatever
    /// happens next. Walk decisions and `ResyncRequired` land here.
    pub(crate) fn discard_meta_snapshot(&self) {
        if let Some(meta) = &self.meta {
            meta.discard();
        }
    }

    /// Throw away everything this handle has prefetched or retained, so the
    /// next read goes back to the server.
    ///
    /// For callers that hold a handle open across remote changes. Closing and
    /// reopening the handle would also work, and is what the kernel daemon
    /// used to do — but a handle can own an advisory lock, and dropping it to
    /// refresh some cached bytes would quietly hand that lock to whoever asked
    /// next. This keeps the handle, and with it the lock.
    pub fn invalidate_read_cache(&self, fh: u64) {
        if let Some(state) = self.open_files.get(&fh) {
            state.ra.clear();
            *state.blob.write().unwrap() = None;
        }
    }

    /// True when the auto-cache walk was skipped this mount because the
    /// export tree token still matched what the cache was last complete at.
    /// A mount with no auto-cache never walks, and reports false.
    pub fn auto_cache_walk_skipped(&self) -> bool {
        self.cache.as_ref().is_some_and(|c| c.walk_skipped())
    }

    /// The cached attribute for `ino`, if one is present and inside the TTL.
    ///
    /// Deliberately does NOT fetch on a miss: the callers are the ones deciding
    /// whether they can avoid talking to the server at all, and a helper that
    /// silently round-trips would defeat the whole point of asking.
    pub(super) fn cached_attr_fresh(&self, ino: u64) -> Option<Attr> {
        let hit = self.attr_cache.get(&ino)?;
        let (attr, when) = *hit;
        (when.elapsed() < self.attr_ttl()).then_some(attr)
    }

    /// Invalidate the cache entry, readahead windows, and every open fh's
    /// fast path for `path`.
    /// Drop whatever open handles on `path` have prefetched or retained,
    /// after a change that did not come from this client.
    ///
    /// `mark_path_written` does this for our own writes. Remote changes arrive
    /// through the event pump instead, and nothing did it there: the attr
    /// cache and the listing were busted — so `stat` told the truth — while
    /// the bytes an open handle would serve still came from blocks fetched
    /// before the change.
    ///
    /// The retained blocks are the sharp part. A block retained SHORT because
    /// it was the tail of the file goes on answering after the file grows, and
    /// the read returns zero bytes. The FUSE kernel, seeing an offset inside
    /// i_size, reads that short reply as a hole and ZERO-FILLS the page, so
    /// the application gets NULs rather than an error or the appended data,
    /// for the life of the descriptor. `tail -f` across two machines is the
    /// ordinary way to meet it.
    pub(crate) fn invalidate_open_reads(&self, path: &RelPath) {
        for entry in self.open_files.iter() {
            if entry.value().path == *path {
                entry.value().cache_ok.store(false, Ordering::Relaxed);
                entry.value().ra.clear();
                *entry.value().blob.write().unwrap() = None;
            }
        }
    }

    /// The same, for every open handle: a resync means nothing is trusted.
    pub(crate) fn invalidate_all_open_reads(&self) {
        for entry in self.open_files.iter() {
            entry.value().cache_ok.store(false, Ordering::Relaxed);
            entry.value().ra.clear();
            *entry.value().blob.write().unwrap() = None;
        }
    }

    pub(super) fn mark_path_written(&self, path: &RelPath, fresh: Option<(u64, Attr)>) {
        self.invalidate_open_reads(path);
        // The parent's listings carry this file's size and mtime, and our own
        // write will never echo back as an event to correct them. With the
        // reply's attributes in hand (v5+), the entry is PATCHED to the new
        // truth; without them the warm listing has to go — it has no TTL to
        // age the stale size out — while the live dir_cache is left to its
        // TTL, the trade writes have always made.
        match fresh {
            Some((ino, attr)) => self.patch_parent_dir(path, ListingPatch::Upsert(ino, attr)),
            None => {
                if let Some((parent, _)) = path.split() {
                    self.bust_warm(&parent);
                }
            }
        }
        if let Some(cache) = &self.cache {
            cache.invalidate(path);
        }
    }
}

/// One entry's worth of change to a cached directory listing.
pub(crate) enum ListingPatch {
    /// Insert or replace `child`'s entry, attributes from the mutation's own
    /// reply.
    Upsert(u64, Attr),
    /// Drop `child`'s entry.
    Remove,
}

impl RemoteFs {
    /// Patch the PARENT's cached listings after one of OUR OWN mutations,
    /// instead of dropping them.
    ///
    /// Self-origin events are stripped server-side, so the pump never
    /// corrects our own changes — something synchronous has to. That used to
    /// be invalidation, and invalidation is what made write bursts pay wire
    /// round trips re-learning their own work: the Windows mount probes
    /// every name before creating it, a probe is only local while a COMPLETE
    /// listing is cached, and the previous create had just thrown that
    /// listing away. Measured over a 10-file create burst on the loopback
    /// harness: 39 wire requests invalidating, 20 patching — and on a ~93 ms
    /// WAN link the difference was a third of the entire cost of writing
    /// small files.
    ///
    /// Patching preserves the invariant that makes listings answer negative
    /// lookups: the mutation's reply names exactly one entry and carries its
    /// attributes, so completeness survives. Both epochs still advance —
    /// a listing fetched BEFORE this mutation must not land after it and
    /// claim a completeness it lacks, the same in-flight guard invalidation
    /// always provided. (Renames still invalidate: two listings and a moved
    /// name are not "one entry's worth of change".)
    pub(crate) fn patch_parent_dir(&self, child: &RelPath, patch: ListingPatch) {
        let Some((parent, name)) = child.split() else {
            return;
        };
        self.dir_epoch.fetch_add(1, Ordering::Release);
        self.warm_epoch.fetch_add(1, Ordering::Release);
        if let Some(pino) = self.ino.ino_of(&parent) {
            if let Some(mut hit) = self.dir_cache.get_mut(&pino) {
                let (listing, _) = hit.value_mut();
                match &patch {
                    ListingPatch::Upsert(ino, attr) => {
                        match listing.iter_mut().find(|(n, _, _)| n == name) {
                            Some(e) => {
                                e.1 = *ino;
                                e.2 = *attr;
                            }
                            None => {
                                // Listings are served name-ordered; keep them so.
                                let at = listing.partition_point(|(n, _, _)| n.as_str() < name);
                                listing.insert(at, (name.to_string(), *ino, *attr));
                            }
                        }
                    }
                    ListingPatch::Remove => listing.retain(|(n, _, _)| n != name),
                }
            }
        }
        match self.warm.get_mut(&parent) {
            Some(mut w) => {
                match &patch {
                    ListingPatch::Upsert(_, attr) => match w.iter_mut().find(|(n, _)| n == name) {
                        Some(e) => e.1 = *attr,
                        None => {
                            let at = w.partition_point(|(n, _)| n.as_str() < name);
                            w.insert(at, (name.to_string(), *attr));
                        }
                    },
                    ListingPatch::Remove => w.retain(|(n, _)| n != name),
                }
                tracing::debug!(parent = %parent, name, "patch_parent_dir: warm PATCHED");
            }
            None => {
                tracing::debug!(parent = %parent, name, "patch_parent_dir: warm ABSENT");
            }
        }
    }
}
