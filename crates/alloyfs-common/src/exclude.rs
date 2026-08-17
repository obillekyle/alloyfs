//! Gitignore-flavored exclude matching over `RelPath`s.
//!
//! Pattern semantics (each user pattern `p` expands to several globs):
//! - `p` matches the path itself, `p/**` gives directory-prefix semantics
//!   (excluding `secrets` excludes everything under it);
//! - a bare name (no `/`) also matches at any depth: `**/p`, `**/p/**` —
//!   so `node_modules` means every node_modules anywhere, like gitignore.
//!
//! The root path is never excluded. Compilation errors are surfaced (bad
//! globs must fail startup/CLI, never be silently ignored).
//!
//! Single shared copy: the server and client sit on opposite ends of the wire
//! and MUST agree on these semantics — living here makes drift impossible.

use alloyfs_proto::RelPath;
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

/// Bookkeeping every OS scatters into a directory it touches, which is about
/// the machine and not about the data.
///
/// These cross the wire in the worst direction: mounting an export on Windows
/// makes the *mounting* machine's volume service create `System Volume
/// Information` inside somebody else's folder, and a recycle bin follows the
/// first delete. That was found in a real first-run — a Linux `~/webdav`
/// grew a `System Volume Information` directory the moment it appeared as a
/// drive letter, and it is also served over WebDAV to things that then have
/// to explain it.
///
/// Excluded in both directions and by default, because there is no
/// arrangement of two machines where one's recycle bin is the other's
/// business. Turn them back on per-export with `default_excludes: false` if
/// you are deliberately backing up a whole volume.
///
/// Matched case-insensitively everywhere, including on Linux: these names are
/// created by case-insensitive filesystems and their casing genuinely varies
/// across Windows versions (`$RECYCLE.BIN` and `$Recycle.Bin` both occur).
pub const LOCAL_ARTIFACTS: &[&str] = &[
    // Windows
    "System Volume Information",
    "$RECYCLE.BIN",
    "RECYCLER",
    "Thumbs.db",
    "ehthumbs.db",
    "desktop.ini",
    // macOS
    ".DS_Store",
    ".Spotlight-V100",
    ".Trashes",
    ".fseventsd",
    ".TemporaryItems",
    ".DocumentRevisions-V100",
    ".AppleDouble",
    // Linux
    "lost+found",
    ".Trash-*",
];

#[derive(Clone)]
pub struct ExcludeSet {
    set: GlobSet,
    empty: bool,
    /// Always-on, always case-insensitive. Kept separate from `set` so the
    /// user's patterns keep the case semantics of their own platform.
    artifacts: Option<GlobSet>,
}

impl ExcludeSet {
    /// User patterns plus [`LOCAL_ARTIFACTS`]. What exports and mounts want:
    /// the caller's rules, and never anybody's recycle bin.
    pub fn compile_with_defaults(patterns: &[String], case_insensitive: bool) -> anyhow::Result<Self> {
        let mut set = Self::compile(patterns, case_insensitive)?;
        let owned: Vec<String> = LOCAL_ARTIFACTS.iter().map(|s| s.to_string()).collect();
        set.artifacts = Some(Self::compile(&owned, true)?.set);
        Ok(set)
    }

    pub fn compile(patterns: &[String], case_insensitive: bool) -> anyhow::Result<Self> {
        let mut builder = GlobSetBuilder::new();
        for p in patterns {
            let p = p.trim_end_matches('/');
            anyhow::ensure!(!p.is_empty(), "empty exclude pattern");
            let mut variants = vec![p.to_string(), format!("{p}/**")];
            if !p.contains('/') {
                variants.push(format!("**/{p}"));
                variants.push(format!("**/{p}/**"));
            }
            for v in variants {
                let glob = GlobBuilder::new(&v)
                    .literal_separator(true)
                    .case_insensitive(case_insensitive)
                    .build()
                    .map_err(|e| anyhow::anyhow!("bad exclude pattern {p:?}: {e}"))?;
                builder.add(glob);
            }
        }
        Ok(Self {
            set: builder.build()?,
            empty: patterns.is_empty(),
            artifacts: None,
        })
    }

    pub fn is_excluded(&self, path: &RelPath) -> bool {
        if path.is_root() {
            return false;
        }
        if let Some(artifacts) = &self.artifacts {
            if artifacts.is_match(&path.0) {
                return true;
            }
        }
        if self.empty {
            return false;
        }
        self.set.is_match(&path.0)
    }

    /// Whether the caller supplied any patterns. Deliberately ignores the
    /// always-on defaults: this answers "did the user configure excludes",
    /// which is what the overlay uses to decide whether to exist at all.
    pub fn is_empty(&self) -> bool {
        self.empty
    }
}

impl Default for ExcludeSet {
    fn default() -> Self {
        Self::compile(&[], false).expect("empty exclude set always compiles")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(patterns: &[&str], ci: bool) -> ExcludeSet {
        let owned: Vec<String> = patterns.iter().map(|s| s.to_string()).collect();
        ExcludeSet::compile(&owned, ci).unwrap()
    }

    fn x(s: &ExcludeSet, p: &str) -> bool {
        s.is_excluded(&RelPath(p.to_string()))
    }

    #[test]
    fn bare_name_matches_any_depth_and_children() {
        let s = set(&["node_modules"], false);
        assert!(x(&s, "node_modules"));
        assert!(x(&s, "node_modules/left-pad/index.js"));
        assert!(x(&s, "app/node_modules"));
        assert!(x(&s, "app/deep/node_modules/x/y"));
        assert!(!x(&s, "app/node_modules_backup"));
        assert!(!x(&s, "src/main.rs"));
    }

    #[test]
    fn pathed_pattern_is_anchored() {
        let s = set(&["build/out"], false);
        assert!(x(&s, "build/out"));
        assert!(x(&s, "build/out/a.o"));
        assert!(!x(&s, "app/build/out"));
    }

    #[test]
    fn glob_meta_and_dir_prefix() {
        let s = set(&["*.tmp", "**/.git"], false);
        assert!(x(&s, "a.tmp"));
        assert!(x(&s, "deep/dir/b.tmp"));
        assert!(x(&s, ".git"));
        assert!(x(&s, "repo/.git/objects/ab"));
        assert!(!x(&s, "repo/.github/workflows"));
    }

    #[test]
    fn case_flag() {
        let ci = set(&["Node_Modules"], true);
        assert!(x(&ci, "node_modules/x"));
        let cs = set(&["Node_Modules"], false);
        assert!(!x(&cs, "node_modules/x"));
    }

    #[test]
    fn root_never_excluded_and_empty_set() {
        let s = set(&["**"], false);
        assert!(!s.is_excluded(&RelPath(String::new())));
        let e = ExcludeSet::default();
        assert!(!x(&e, "anything"));
        assert!(e.is_empty());
    }

    #[test]
    fn bad_pattern_errors() {
        assert!(ExcludeSet::compile(&["a[".to_string()], false).is_err());
        assert!(ExcludeSet::compile(&["".to_string()], false).is_err());
    }
}

#[cfg(test)]
mod artifact_tests {
    use super::*;

    fn p(s: &str) -> RelPath {
        RelPath(s.to_string())
    }

    /// The case that prompted this: a Windows client mounting a Linux export
    /// makes its own volume service create these inside the served folder.
    #[test]
    fn os_bookkeeping_is_excluded_by_default() {
        let set = ExcludeSet::compile_with_defaults(&[], false).unwrap();
        for path in [
            "System Volume Information",
            "System Volume Information/tracking.log",
            "$RECYCLE.BIN",
            "$RECYCLE.BIN/S-1-5-21/file",
            "sub/dir/.DS_Store",
            "lost+found",
            ".Trash-1000/expunged",
            "Thumbs.db",
            "deep/nested/desktop.ini",
        ] {
            assert!(set.is_excluded(&p(path)), "{path} should be hidden");
        }
    }

    /// Matched case-insensitively EVEN on a case-sensitive server, because the
    /// casing is Windows's choice and it varies between versions. A Linux
    /// export compiles its own patterns case-sensitively, so this has to be
    /// asserted separately rather than assumed from the flag.
    #[test]
    fn artifact_casing_does_not_matter_on_a_case_sensitive_server() {
        let set = ExcludeSet::compile_with_defaults(&[], false).unwrap();
        for path in [
            "$Recycle.Bin",
            "$recycle.bin/x",
            "system volume information",
            "SUB/thumbs.DB",
        ] {
            assert!(set.is_excluded(&p(path)), "{path} should be hidden");
        }
    }

    /// Real data that merely resembles an artifact name stays visible —
    /// hiding someone's actual file would be worse than the problem.
    #[test]
    fn lookalikes_are_not_swallowed() {
        let set = ExcludeSet::compile_with_defaults(&[], false).unwrap();
        for path in [
            "System Volume Information Notes.txt",
            "my-lost+found-notes",
            "docs/desktop.ini.bak",
            "Thumbs.db.old",
        ] {
            assert!(!set.is_excluded(&p(path)), "{path} must stay visible");
        }
    }

    /// `is_empty` reports whether the USER configured anything. The overlay
    /// uses it to decide whether to exist at all, and an overlay conjured into
    /// being by the built-in defaults would be a surprise.
    #[test]
    fn built_in_defaults_do_not_count_as_user_patterns() {
        let set = ExcludeSet::compile_with_defaults(&[], false).unwrap();
        assert!(set.is_empty());
        assert!(set.is_excluded(&p(".DS_Store")), "still filtered though");

        let plain = ExcludeSet::compile(&[], false).unwrap();
        assert!(!plain.is_excluded(&p(".DS_Store")), "opt-out really opts out");
    }
}
