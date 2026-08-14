//! The pure core of filesystem-event coalescing, shared by the agent's
//! watcher (server side) and the sync engine's local watcher (client side).
//! One copy — both ends must collapse event storms identically.
//!
//! Real filesystems storm: a compiler writing one artifact emits dozens of
//! raw events in milliseconds. `Coalescer` collapses them per-path
//! (Created+Modified→Created, Created+Removed→nothing, …), pairs renames,
//! and degrades events across an exclude boundary to their visible half.
//! The caller owns the pacing (debounce timers, overflow flushes) — this
//! type is synchronous state, no tokio.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ds_proto::{EventKind, RelPath};
use notify::event::{CreateKind, EventKind as NotifyKind, ModifyKind, RemoveKind, RenameMode};

use crate::ExcludeSet;

/// What we remember about a path between flushes.
#[derive(Clone, Debug)]
struct Pending {
    kind: EventKind,
    first_seen: Instant,
}

pub struct Coalescer {
    root: PathBuf,
    exclude: ExcludeSet,
    pending: HashMap<RelPath, Pending>,
    renames: Vec<(RelPath, RelPath)>, // (from, to), flushed with the batch
}

impl Coalescer {
    pub fn new(root: PathBuf, exclude: ExcludeSet) -> Self {
        Self {
            root,
            exclude,
            pending: HashMap::new(),
            renames: Vec::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty() && self.renames.is_empty()
    }

    pub fn len(&self) -> usize {
        self.pending.len() + self.renames.len()
    }

    /// Age of the oldest pending change (overflow-flush input).
    pub fn oldest_age(&self) -> Option<Duration> {
        self.pending.values().map(|p| p.first_seen.elapsed()).min()
    }

    /// Fold one raw notify event into the pending state.
    pub fn ingest(&mut self, event: notify::Event) {
        match event.kind {
            NotifyKind::Create(CreateKind::Any | CreateKind::File | CreateKind::Folder | _) => {
                for p in &event.paths {
                    if let Some(rel) = rel_of(&self.root, p) {
                        self.push(rel, EventKind::Created);
                    }
                }
            }
            NotifyKind::Modify(ModifyKind::Name(mode)) => match (mode, event.paths.as_slice()) {
                (RenameMode::Both, [from, to]) => {
                    if let (Some(f), Some(t)) = (rel_of(&self.root, from), rel_of(&self.root, to)) {
                        // Renames across the exclude boundary degrade to the
                        // visible half: into-excluded looks like a removal,
                        // out-of-excluded looks like a creation.
                        let fx = self.exclude.is_excluded(&f);
                        let tx = self.exclude.is_excluded(&t);
                        match (fx, tx) {
                            (true, true) => {}
                            (true, false) => self.push(t, EventKind::Created),
                            (false, true) => self.push(f, EventKind::Removed),
                            (false, false) => self.renames.push((f, t)),
                        }
                    }
                }
                // Unpaired halves degrade to Remove/Create — still correct for
                // cache invalidation, just less precise.
                (RenameMode::From, [from]) => {
                    if let Some(f) = rel_of(&self.root, from) {
                        self.push(f, EventKind::Removed);
                    }
                }
                (RenameMode::To, [to]) => {
                    if let Some(t) = rel_of(&self.root, to) {
                        self.push(t, EventKind::Created);
                    }
                }
                _ => {
                    for p in &event.paths {
                        if let Some(rel) = rel_of(&self.root, p) {
                            self.push(rel, EventKind::Modified);
                        }
                    }
                }
            },
            NotifyKind::Modify(ModifyKind::Metadata(_)) => {
                for p in &event.paths {
                    if let Some(rel) = rel_of(&self.root, p) {
                        self.push(rel, EventKind::AttrChanged);
                    }
                }
            }
            NotifyKind::Modify(_) => {
                for p in &event.paths {
                    if let Some(rel) = rel_of(&self.root, p) {
                        self.push(rel, EventKind::Modified);
                    }
                }
            }
            NotifyKind::Remove(RemoveKind::Any | RemoveKind::File | RemoveKind::Folder | _) => {
                for p in &event.paths {
                    if let Some(rel) = rel_of(&self.root, p) {
                        self.push(rel, EventKind::Removed);
                    }
                }
            }
            NotifyKind::Any | NotifyKind::Access(_) | NotifyKind::Other => {}
        }
    }

    /// Drain everything into one ordered batch: paired renames first (their
    /// degraded Removed/Created halves deduped — a real Modified on the
    /// target survives), then remaining items in ascending path order
    /// (parents before children; deterministic for tests and debugging).
    pub fn take_batch(&mut self) -> Vec<(RelPath, EventKind)> {
        let mut batch: Vec<(RelPath, EventKind)> = Vec::with_capacity(self.len());
        for (from, to) in self.renames.drain(..) {
            // Some backends report one rename as From + To + Both: the paired
            // event wins, so drop the degraded halves it would duplicate —
            // but ONLY exact halves.
            if self
                .pending
                .get(&from)
                .is_some_and(|p| matches!(p.kind, EventKind::Removed))
            {
                self.pending.remove(&from);
            }
            if self
                .pending
                .get(&to)
                .is_some_and(|p| matches!(p.kind, EventKind::Created))
            {
                self.pending.remove(&to);
            }
            batch.push((from, EventKind::RenamedFrom { to }));
        }
        let mut items: Vec<(RelPath, Pending)> = self.pending.drain().collect();
        items.sort_by(|a, b| a.0.cmp(&b.0));
        for (path, p) in items {
            batch.push((path, p.kind));
        }
        batch
    }

    fn push(&mut self, path: RelPath, kind: EventKind) {
        if self.exclude.is_excluded(&path) {
            return;
        }
        match merge(self.pending.get(&path).map(|p| &p.kind), kind) {
            Some(kind) => {
                let first_seen = self
                    .pending
                    .get(&path)
                    .map(|p| p.first_seen)
                    .unwrap_or_else(Instant::now);
                self.pending.insert(path, Pending { kind, first_seen });
            }
            None => {
                self.pending.remove(&path);
            }
        }
    }
}

fn merge(old: Option<&EventKind>, new: EventKind) -> Option<EventKind> {
    use EventKind::*;
    Some(match (old, new) {
        // A file that appeared and was then modified is still just "Created"
        // from any observer's point of view.
        (Some(Created), Modified | AttrChanged) => Created,
        // Appeared then vanished: nothing happened, observably.
        (Some(Created), Removed) => return None,
        // Vanished then reappeared: net effect is content replacement.
        (Some(Removed), Created) => Modified,
        // Attr change then data change: report the stronger one.
        (Some(AttrChanged), Modified) => Modified,
        (Some(Modified), AttrChanged) => Modified,
        // Renames and resyncs always win outright.
        (_, ev @ (RenamedFrom { .. } | ResyncRequired)) => ev,
        (Some(old), _) => old.clone(),
        (None, ev) => ev,
    })
}

/// Convert an absolute path inside `root` to wire form.
pub fn rel_of(root: &Path, abs: &Path) -> Option<RelPath> {
    let rel = abs.strip_prefix(root).ok()?;
    let s = rel.to_str()?;
    if s.is_empty() {
        return None; // events on the root itself aren't interesting
    }
    Some(RelPath(s.replace('\\', "/")))
}
