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

use alloyfs_proto::{EventKind, RelPath};
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
    /// Endpoints of already-paired renames in this window. Some backends
    /// (Linux inotify) report one rename THREE times — From, To, AND Both;
    /// once the pair is known, late-arriving halves must be dropped at
    /// ingest or a subsequent real Modified on the target merges into the
    /// half's Created and gets eaten by the flush-time dedupe.
    renamed_from: std::collections::HashSet<RelPath>,
    renamed_to: std::collections::HashSet<RelPath>,
}

impl Coalescer {
    pub fn new(root: PathBuf, exclude: ExcludeSet) -> Self {
        Self {
            root,
            exclude,
            pending: HashMap::new(),
            renames: Vec::new(),
            renamed_from: std::collections::HashSet::new(),
            renamed_to: std::collections::HashSet::new(),
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
                            (false, false) => {
                                // Purge halves that already arrived, so a
                                // LATER Modified on `t` starts clean and
                                // survives the flush-time dedupe.
                                if self
                                    .pending
                                    .get(&f)
                                    .is_some_and(|p| matches!(p.kind, EventKind::Removed))
                                {
                                    self.pending.remove(&f);
                                }
                                if self
                                    .pending
                                    .get(&t)
                                    .is_some_and(|p| matches!(p.kind, EventKind::Created))
                                {
                                    self.pending.remove(&t);
                                }
                                self.renamed_from.insert(f.clone());
                                self.renamed_to.insert(t.clone());
                                self.renames.push((f, t));
                            }
                        }
                    }
                }
                // Unpaired halves degrade to Remove/Create — still correct for
                // cache invalidation, just less precise. Halves of a rename
                // ALREADY paired in this window are duplicates: drop them.
                (RenameMode::From, [from]) => {
                    if let Some(f) = rel_of(&self.root, from) {
                        if !self.renamed_from.contains(&f) {
                            self.push(f, EventKind::Removed);
                        }
                    }
                }
                (RenameMode::To, [to]) => {
                    if let Some(t) = rel_of(&self.root, to) {
                        if !self.renamed_to.contains(&t) {
                            self.push(t, EventKind::Created);
                        }
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
        self.renamed_from.clear();
        self.renamed_to.clear();
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
        // ANY other state followed by a removal is a removal — the file is
        // gone, and reporting the earlier Modified instead would leave every
        // observer believing it still exists. (A modify-then-delete inside
        // one debounce window is exactly what a rename's degraded halves
        // look like when a write preceded them.)
        (Some(_), Removed) => Removed,
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

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{DataChange, EventKind as NK, ModifyKind as MK, RenameMode};

    fn co() -> Coalescer {
        Coalescer::new(PathBuf::from("/x"), ExcludeSet::compile(&[], false).unwrap())
    }

    fn ev(kind: NK, paths: &[&str]) -> notify::Event {
        notify::Event {
            kind,
            paths: paths.iter().map(|p| PathBuf::from(format!("/x/{p}"))).collect(),
            attrs: Default::default(),
        }
    }

    /// Linux inotify reports one rename THREE times (From, To, Both). A real
    /// Modified on the target arriving after all three must survive — this
    /// exact sequence used to merge into the To-half's Created and get eaten
    /// by the rename dedupe (found by the sync battery on Linux).
    #[test]
    fn triple_reported_rename_keeps_later_modify() {
        let mut c = co();
        c.ingest(ev(NK::Modify(MK::Name(RenameMode::From)), &["a.txt"]));
        c.ingest(ev(NK::Modify(MK::Name(RenameMode::To)), &["b.txt"]));
        c.ingest(ev(NK::Modify(MK::Name(RenameMode::Both)), &["a.txt", "b.txt"]));
        c.ingest(ev(NK::Modify(MK::Data(DataChange::Any)), &["b.txt"]));

        let batch = c.take_batch();
        assert_eq!(batch.len(), 2, "rename + modify, got {batch:?}");
        assert!(matches!(&batch[0], (p, EventKind::RenamedFrom { to }) if p.0 == "a.txt" && to.0 == "b.txt"));
        assert!(
            matches!(&batch[1], (p, EventKind::Modified) if p.0 == "b.txt"),
            "the later Modified must survive: {batch:?}"
        );
    }

    /// A delete must survive an earlier modify in the same window: the file
    /// is gone, and reporting Modified would leave observers believing it
    /// still exists. This is what a rename's degraded halves look like when
    /// a write preceded them (found by the sync battery on Windows).
    #[test]
    fn removal_wins_over_earlier_modify() {
        let mut c = co();
        c.ingest(ev(NK::Modify(MK::Data(DataChange::Any)), &["a.txt"]));
        c.ingest(ev(NK::Remove(notify::event::RemoveKind::File), &["a.txt"]));
        let batch = c.take_batch();
        assert_eq!(batch.len(), 1, "{batch:?}");
        assert!(
            matches!(&batch[0], (p, EventKind::Removed) if p.0 == "a.txt"),
            "{batch:?}"
        );
    }

    /// But a create followed by a delete is still nothing at all.
    #[test]
    fn create_then_remove_is_silent() {
        let mut c = co();
        c.ingest(ev(NK::Create(notify::event::CreateKind::File), &["t.txt"]));
        c.ingest(ev(NK::Remove(notify::event::RemoveKind::File), &["t.txt"]));
        assert!(c.take_batch().is_empty());
    }

    /// Same triple-report, halves arriving AFTER the pair: still one event.
    #[test]
    fn triple_reported_rename_halves_after_pair() {
        let mut c = co();
        c.ingest(ev(NK::Modify(MK::Name(RenameMode::Both)), &["a.txt", "b.txt"]));
        c.ingest(ev(NK::Modify(MK::Name(RenameMode::From)), &["a.txt"]));
        c.ingest(ev(NK::Modify(MK::Name(RenameMode::To)), &["b.txt"]));
        c.ingest(ev(NK::Modify(MK::Data(DataChange::Any)), &["b.txt"]));

        let batch = c.take_batch();
        assert_eq!(batch.len(), 2, "rename + modify, got {batch:?}");
        assert!(
            matches!(&batch[1], (p, EventKind::Modified) if p.0 == "b.txt"),
            "late halves must not shadow the Modified: {batch:?}"
        );
    }
}
