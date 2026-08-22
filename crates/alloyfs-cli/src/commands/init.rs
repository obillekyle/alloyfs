//! `alloyfs init` — write a config for the directory you are standing in.
//!
//! The starter config that `serve` creates on first run is generic: it has an
//! empty `exports` map and a commented example. That is the right thing when
//! nobody asked for it, but when someone runs `init` inside a project they
//! have already told us what they want to export.

use std::path::{Path, PathBuf};

/// Derive an export name from a directory. Export names end up in URLs and in
/// on-disk paths under `~/.alloyfs`, so anything outside a conservative set is
/// replaced rather than escaped.
fn export_name_for(dir: &Path) -> String {
    let raw = dir
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("export")
        .to_lowercase();
    let cleaned: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "export".to_string()
    } else {
        trimmed
    }
}

/// Path as it should appear in the config file.
///
/// Two Windows-shaped problems, both of which produce a file that looks fine
/// and is wrong:
///
/// * `canonicalize` returns the extended-length form, `\\?\C:\Users\...`.
///   Written into a config it becomes `//?/C:/Users/...`, which is not a path
///   anyone would recognise and is not what the user asked to export.
/// * YAML treats a backslash in a double-quoted scalar as an escape, so
///   `C:\Users\...` would fail to parse or silently lose characters. Windows
///   accepts the forward-slash form everywhere, so use it.
fn config_path(path: &Path) -> String {
    let s = path.display().to_string();
    // `\\?\UNC\server\share` is left alone: stripping only the prefix would
    // turn it into the non-existent path `UNC\server\share`.
    let s = match s.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC\\") => rest.to_string(),
        _ => s,
    };
    s.replace('\\', "/")
}

fn render(name: &str, path: &Path) -> String {
    let path = config_path(path);
    format!(
        "\
# AlloyFS configuration, written by `alloyfs init`.
#
# Serve it:      alloyfs serve        (run from this directory)
# Mount it:      alloyfs mount tcp://127.0.0.1:7440/{name} /some/mountpoint
#
# `alloyfs serve` picks this up automatically when run from here, the way
# cargo finds Cargo.toml. Elsewhere, pass --config, or keep the per-user one
# at ~/.alloyfs/config.yml. The agent logs which config it loaded on startup.

version: 3

# What this machine offers to others.
server:
  # A non-loopback address REQUIRES tcp_token; the agent refuses to start
  # without one rather than exposing every export to the network.
  tcp_listen: \"127.0.0.1:7440\"
  # tcp_token: \"change-me\"
  # http_listen: \"127.0.0.1:7441\"
  # http_token: \"change-me\"
  exports:
    {name}:
      path: \"{path}\"
      read_only: false
      exclude:
        - \"**/.git\"
      # Settings suggested to anyone who mounts this export. Clients merge
      # them under their own, and may ignore them with
      # --no-server-defaults.
      client:
        exclude:
          - node_modules
          - target
        auto_cache_max: 2M

# What this machine mounts from others. `alloyfs start` runs every entry.
#
# client:
#   mounts:
#     work:
#       url: ssh://somehost/{name}
#       at: \"/mnt/work\"          # or a drive letter on Windows: \"W:\"
"
    )
}

pub fn run(dir: Option<PathBuf>, name: Option<String>, global: bool, force: bool) -> anyhow::Result<()> {
    let dir = match dir {
        Some(d) => d,
        None => std::env::current_dir()?,
    };
    // Absolute, because the config may be read from anywhere: a relative path
    // would resolve against whatever directory the agent happened to start in.
    let dir =
        std::fs::canonicalize(&dir).map_err(|e| anyhow::anyhow!("cannot resolve {}: {e}", dir.display()))?;
    anyhow::ensure!(dir.is_dir(), "{} is not a directory", dir.display());

    let name = name.unwrap_or_else(|| export_name_for(&dir));
    let target = if global {
        crate::config::app_dir().join("config.yml")
    } else {
        std::env::current_dir()?.join("alloyfs.yml")
    };

    if target.exists() && !force {
        anyhow::bail!(
            "{} already exists. Pass --force to overwrite it.",
            target.display()
        );
    }

    std::fs::write(&target, render(&name, &dir))?;

    println!("wrote {}", target.display());
    // The cleaned form here too: printing `\\?\C:\...` at someone is just as
    // unhelpful as writing it into the file.
    println!("  export {name} -> {}", config_path(&dir));
    println!();
    if global {
        println!("`alloyfs serve` will find it from anywhere.");
    } else {
        println!("Serve it with:");
        println!("  alloyfs serve            (from this directory)");
        println!("  alloyfs serve --config {}   (from anywhere)", target.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_names_are_derived_from_the_directory() {
        assert_eq!(export_name_for(Path::new("/home/you/projects")), "projects");
        assert_eq!(export_name_for(Path::new("/home/you/My Docs")), "my-docs");
    }

    /// Names reach URLs and on-disk paths, so anything that could change how
    /// either is parsed becomes a dash rather than being passed through.
    #[test]
    fn awkward_directory_names_are_cleaned() {
        assert_eq!(export_name_for(Path::new("/tmp/a/b c")), "b-c");
        assert_eq!(export_name_for(Path::new("/tmp/a/../weird")), "weird");
        assert_eq!(export_name_for(Path::new("/tmp/a/...")), "export");
        assert_eq!(export_name_for(Path::new("/tmp/a/über")), "ber");
    }

    /// `canonicalize` hands back `\\?\C:\...` on Windows. Written verbatim
    /// that becomes `//?/C:/...`, which is not a path anyone would recognise —
    /// and it shipped that way until a real run showed it.
    #[test]
    fn the_extended_length_prefix_is_stripped() {
        assert_eq!(
            config_path(Path::new(r"\\?\C:\Users\Kyle\projects")),
            "C:/Users/Kyle/projects"
        );
        // A UNC path keeps its prefix: stripping it would leave the
        // non-existent `UNC/server/share`.
        assert_eq!(
            config_path(Path::new(r"\\?\UNC\server\share")),
            "//?/UNC/server/share"
        );
        assert_eq!(config_path(Path::new("/home/you/projects")), "/home/you/projects");
    }

    /// A Windows path in a double-quoted YAML scalar would treat backslashes
    /// as escapes; the rendered config has to be loadable.
    #[test]
    fn a_windows_path_renders_as_loadable_yaml() {
        let out = render("proj", Path::new(r"C:\Users\Kyle\projects"));
        assert!(out.contains("C:/Users/Kyle/projects"), "{out}");
        assert!(!out.contains(r"C:\Users"));
        let parsed: serde_yaml::Value = serde_yaml::from_str(&out).expect("valid YAML");
        assert_eq!(
            parsed["server"]["exports"]["proj"]["path"].as_str(),
            Some("C:/Users/Kyle/projects")
        );
    }

    /// What `init` writes must be what the loader ACCEPTS AS-IS.
    ///
    /// It used to render the pre-v3 layout, so the very next command that
    /// read the file classified it as legacy, upgraded it in place, left a
    /// `.bak`, and prepended an "upgraded automatically" header — throwing
    /// away the comments `init` had just written to explain the file. The
    /// blessed way to start produced a file the tool immediately rewrote.
    #[test]
    fn the_rendered_config_is_already_v3_and_needs_no_upgrade() {
        let out = render("projects", Path::new("/home/you/projects"));
        let value: serde_yaml::Value = serde_yaml::from_str(&out).expect("valid YAML");
        assert!(
            matches!(
                crate::config::detect::shape_of(&value).expect("a recognised shape"),
                crate::config::detect::Shape::V3
            ),
            "init must write v3, or the next load rewrites the file it just wrote"
        );
        // ...and it is the real schema, not merely v3-shaped: an unknown key
        // is a hard error here, so this also proves every key is one the
        // loader knows.
        let cfg: crate::config::Config = serde_yaml::from_value(value).expect("loads as v3");
        let exports = cfg
            .server
            .and_then(|s| s.exports)
            .expect("the template defines an export");
        assert!(exports.contains_key("projects"));
    }
}
