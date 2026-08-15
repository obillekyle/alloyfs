//! Rewriting symlink targets that point back into the mount.
//!
//! Not a Windows concern despite where it started: `ln -s "$(pwd)/real.txt"`
//! produces the same absolute-target problem on Unix.

use alloyfs_proto::RelPath;

/// Rewrite `target` relative to `link_dir` when it points back into the mount
/// rooted at `mount_root`; otherwise return it unchanged.
///
/// A free function so it can be tested without standing up a connection — the
/// version this replaced lived in the WinFsp backend and had no direct tests
/// at all.
pub(crate) fn localize_symlink_target(mount_root: &str, target: &str, link_dir: &RelPath) -> String {
    let norm = target.replace('\\', "/");
    // Case-insensitive: Windows drive letters and paths are, and a missed
    // match here just means no rewrite — never a wrong one.
    let prefix = format!("{}/", mount_root.replace('\\', "/").trim_end_matches('/'));
    if !norm.to_uppercase().starts_with(&prefix.to_uppercase()) {
        return target.to_string();
    }
    let rest = norm[prefix.len()..].trim_start_matches('/');

    // Walk up out of the link's directory, then back down to the target; both
    // are export-relative, so the shared prefix cancels.
    let from: Vec<&str> = if link_dir.is_root() {
        Vec::new()
    } else {
        link_dir.0.split('/').collect()
    };
    let to: Vec<&str> = if rest.is_empty() {
        Vec::new()
    } else {
        rest.split('/').collect()
    };
    let shared = from
        .iter()
        .zip(to.iter())
        .take_while(|(a, b)| a.eq_ignore_ascii_case(b))
        .count();
    let mut out: Vec<&str> = vec![".."; from.len() - shared];
    out.extend_from_slice(&to[shared..]);
    if out.is_empty() {
        return ".".to_string(); // the link points at its own directory
    }
    out.join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(s: &str) -> RelPath {
        RelPath(s.to_string())
    }

    /// The case that made this necessary: PowerShell resolves a relative
    /// target to a drive-letter-absolute one before the syscall is made.
    #[test]
    fn a_target_inside_the_mount_becomes_relative() {
        assert_eq!(
            localize_symlink_target("Y:", "Y:\\linktest\\real.txt", &dir("linktest")),
            "real.txt"
        );
        assert_eq!(
            localize_symlink_target("Y:", "Y:\\real.txt", &dir("")),
            "real.txt"
        );
        // Up and back down.
        assert_eq!(
            localize_symlink_target("Y:", "Y:\\real.txt", &dir("sub")),
            "../real.txt"
        );
        assert_eq!(
            localize_symlink_target("Y:", "Y:\\a\\b\\t.txt", &dir("a/c")),
            "../b/t.txt"
        );
    }

    /// The same problem exists on Unix — `ln -s "$(pwd)/real.txt"` — which is
    /// why this does not live in the Windows backend.
    #[test]
    fn a_unix_mountpoint_works_the_same_way() {
        assert_eq!(
            localize_symlink_target("/mnt/alloy", "/mnt/alloy/dir/real.txt", &dir("dir")),
            "real.txt"
        );
        assert_eq!(
            localize_symlink_target("/mnt/alloy/", "/mnt/alloy/real.txt", &dir("sub")),
            "../real.txt"
        );
    }

    /// Absolute somewhere else is left alone — the server refuses it, which is
    /// correct, because it genuinely does leave the export.
    #[test]
    fn targets_outside_the_mount_are_untouched() {
        for t in [
            "C:\\Windows\\system32",
            "/etc/passwd",
            "\\\\server\\share\\x",
            "../already-relative.txt",
            "plain.txt",
        ] {
            assert_eq!(localize_symlink_target("Y:", t, &dir("sub")), t, "{t:?}");
        }
    }

    /// A prefix that only looks like ours must not match: "Y:" should not
    /// swallow "Y:extra" or a different drive.
    #[test]
    fn a_near_miss_prefix_does_not_match() {
        assert_eq!(
            localize_symlink_target("Y:", "Z:/real.txt", &dir("")),
            "Z:/real.txt"
        );
        assert_eq!(
            localize_symlink_target("/mnt/alloy", "/mnt/alloys/x", &dir("")),
            "/mnt/alloys/x"
        );
    }

    #[test]
    fn a_link_to_its_own_directory_is_dot() {
        assert_eq!(localize_symlink_target("Y:", "Y:\\sub", &dir("sub")), ".");
    }
}
