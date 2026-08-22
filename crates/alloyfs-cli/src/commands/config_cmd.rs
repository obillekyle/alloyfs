//! `alloyfs config` — see what the config actually says, before a mount
//! disagrees with what you thought you wrote.
//!
//! The merge is genuinely subtle, and none of it was inspectable. A mount
//! entry inherits `client:` defaults, LISTS REPLACE rather than union
//! (deliberately, so a per-mount `exclude:` can say "not that, this"), CLI
//! flags land on top of the result, and a v2+ server can suggest more
//! underneath it. `ClientSection::resolve` already computes the answer;
//! there was simply no way to ask it what it computed.

/// `config validate`: does the file parse, and what does it describe?
///
/// Parsing IS the check. `deny_unknown_fields` means a typo'd key is a
/// hard error rather than a silently ignored line, so a config that loads
/// is one whose every key reached a struct — and that is what people want
/// to know after an edit.
pub fn validate(path: Option<std::path::PathBuf>) -> anyhow::Result<()> {
    let (found, cfg) = crate::config::load_with_path(path)?;
    let Some(found) = found else {
        anyhow::bail!(
            "no config found.\n\n  \
             `alloyfs init` writes one, or pass --config <path>."
        );
    };
    let exports = cfg
        .server
        .as_ref()
        .and_then(|s| s.exports.as_ref())
        .map(|e| e.len())
        .unwrap_or(0);
    let mounts = cfg
        .client
        .as_ref()
        .map(|c| c.resolved_mounts().len())
        .unwrap_or(0);
    println!("ok  {}", found.display());
    println!("    {exports} export(s), {mounts} mount(s)");
    if exports == 0 && mounts == 0 {
        println!();
        println!("  It parses, but it describes nothing to run.");
        println!("  Add exports under `server:` or mounts under `client.mounts:`.");
    }
    Ok(())
}

/// `config print`: the settled values, after every layer has had its say.
///
/// What it does NOT include is the server's suggestions: those arrive over
/// the wire at mount time from a v2+ agent, so printing them would mean
/// connecting, and this command is for reading a file. The line saying so
/// is part of the output rather than a footnote in the docs — a person
/// comparing this against a running mount needs to know which layer is
/// missing.
pub fn print(path: Option<std::path::PathBuf>, mount: Option<String>) -> anyhow::Result<()> {
    let (found, cfg) = crate::config::load_with_path(path)?;
    let Some(found) = found else {
        anyhow::bail!("no config found. `alloyfs init` writes one, or pass --config <path>.");
    };
    println!("# from {}", found.display());

    if let Some(server) = &cfg.server {
        println!("\nserver:");
        if let Some(listen) = &server.tcp_listen {
            println!("  tcp_listen: {listen}");
        }
        println!("  zstd: {}", server.zstd.unwrap_or(false));
        for (name, export) in server.exports.iter().flatten() {
            println!("  export {name}: {}", export.path.display());
        }
    }

    let resolved = cfg
        .client
        .as_ref()
        .map(|c| c.resolved_mounts())
        .unwrap_or_default();
    let wanted: Vec<_> = match &mount {
        Some(name) => resolved.into_iter().filter(|(n, _)| n == name).collect(),
        None => resolved,
    };
    if let Some(name) = &mount {
        if wanted.is_empty() {
            anyhow::bail!("no mount named {name:?} in {}", found.display());
        }
    }
    for (name, m) in &wanted {
        println!("\nmount {name}:");
        println!("  url: {}", m.url);
        println!("  at: {}", m.at.display());
        println!("  exclude: {:?}", m.exclude);
        println!("  pin: {:?}", m.pin);
        println!("  auto_cache_max: {}", opt(&m.auto_cache_max));
        println!("  auto_cache_budget: {}", opt(&m.auto_cache_budget));
        println!(
            "  data_dir: {}",
            m.data_dir
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "(default)".into())
        );
        println!("  no_server_defaults: {}", m.no_server_defaults);
        println!("  detect_conflicts: {}", m.detect_conflicts);
        println!("  zstd: {}", m.zstd);
        // Never the value: a token is a credential, and a command people
        // paste into issues must not print one.
        println!("  token: {}", if m.token.is_some() { "(set)" } else { "(none)" });
    }

    println!();
    println!("# This is the file's own answer: client defaults merged with each");
    println!("# mount's overrides. CLI flags land on top at mount time, and a v2+");
    println!("# server may suggest excludes, pins and cache sizes underneath —");
    println!("# neither is visible here, because both need the mount to run.");
    Ok(())
}

fn opt<T: std::fmt::Debug>(v: &Option<T>) -> String {
    match v {
        Some(v) => format!("{v:?}"),
        None => "(unset)".to_string(),
    }
}
