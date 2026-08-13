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
//! NOTE: intentionally duplicated between ds-agent and ds-client (~60 lines)
//! rather than adding a shared crate or putting globset into ds-proto.

use ds_proto::RelPath;
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

pub struct ExcludeSet {
    set: GlobSet,
    empty: bool,
}

impl ExcludeSet {
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
        })
    }

    pub fn is_excluded(&self, path: &RelPath) -> bool {
        if self.empty || path.is_root() {
            return false;
        }
        self.set.is_match(&path.0)
    }

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
