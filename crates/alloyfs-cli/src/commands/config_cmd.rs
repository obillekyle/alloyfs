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
    let (found, cfg) = crate::config::load_with_path(path)
        .map_err(|e| crate::exit::Fatal::err(crate::exit::CONFIG, format!("{e:#}")))?;
    let Some(found) = found else {
        return Err(crate::exit::Fatal::err(
            crate::exit::CONFIG,
            "no config found.\n\n  \
             `alloyfs init` writes one, or pass --config <path>.",
        ));
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
    let (found, cfg) = crate::config::load_with_path(path)
        .map_err(|e| crate::exit::Fatal::err(crate::exit::CONFIG, format!("{e:#}")))?;
    let Some(found) = found else {
        return Err(crate::exit::Fatal::err(
            crate::exit::CONFIG,
            "no config found. `alloyfs init` writes one, or pass --config <path>.",
        ));
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

/// Print the JSON Schema for the config file.
///
/// Derived from the same types `serde` deserializes with, which is the whole
/// value of it: a schema maintained separately drifts, and a schema that
/// drifts is worse than none — it underlines valid keys and blesses invalid
/// ones. This one cannot describe a file the binary would reject, because
/// both come from the same struct definitions.
///
/// The complement to the located parse errors: those tell you where you went
/// wrong after the fact, this stops the typo being typed.
pub fn schema() -> anyhow::Result<()> {
    let mut schema = schemars::schema_for!(crate::config::Config);
    // `$id` is what lets an editor associate the schema with a file by URL
    // instead of by local path, and what makes `alloyfs.schema.json` in a
    // repository self-describing.
    schema.insert("$id".into(), "https://alloy.okyle.dev/alloyfs.schema.json".into());
    schema.insert(
        "title".into(),
        format!("AlloyFS config (version {})", crate::config::CURRENT_VERSION).into(),
    );
    println!("{}", serde_json::to_string_pretty(&schema)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    /// The schema must describe the config the parser actually accepts.
    ///
    /// Not a full JSON Schema validation — that would mean a validator
    /// dependency for one test — but a check of the specific claims that go
    /// wrong when a type grows a field or changes shape, each one a case
    /// where an editor would otherwise disagree with the binary.
    #[test]
    fn the_schema_matches_what_the_parser_accepts() {
        let schema = schemars::schema_for!(crate::config::Config);
        let json = serde_json::to_value(&schema).expect("the schema serializes");
        let defs = json["$defs"].as_object().expect("named definitions");

        // Unknown keys are refused by every section, because every section is
        // `deny_unknown_fields`. A schema that allowed them would bless a
        // typo the binary rejects.
        for section in ["ServerSection", "ClientSection", "MountEntry", "ClientDefaults"] {
            assert_eq!(
                defs[section]["additionalProperties"],
                serde_json::json!(false),
                "{section} must refuse unknown keys, as its deserializer does"
            );
        }

        // An export is a path OR a table, and the table half refuses unknown
        // keys even though the derive cannot see the attribute that says so
        // — see ExportConfig::allow_the_short_form.
        let export = &defs["ExportConfig"];
        let forms = export["oneOf"].as_array().expect("both forms of an export");
        assert_eq!(forms.len(), 2, "an export is a path or a table: {export}");
        assert!(
            forms.iter().any(|f| f["type"] == "string"),
            "the short form must be describable: {export}"
        );
        let table = forms
            .iter()
            .find(|f| f["properties"].is_object())
            .expect("the table form");
        assert_eq!(
            table["additionalProperties"],
            serde_json::json!(false),
            "the table form must refuse unknown keys: {table}"
        );
        assert!(
            table["properties"]["path"].is_object(),
            "the table form must have a path: {table}"
        );
    }

    /// Every field the parser knows about reaches the schema.
    ///
    /// The failure this catches is adding a key to a config section and
    /// forgetting the derive, which produces a schema that underlines a
    /// perfectly valid line — worse than no schema, because it is trusted.
    #[test]
    fn every_section_field_is_described() {
        let json = serde_json::to_value(schemars::schema_for!(crate::config::Config))
            .expect("the schema serializes");
        let props = json["properties"].as_object().expect("top-level keys");
        for key in ["version", "server", "client"] {
            assert!(props.contains_key(key), "{key} missing from the schema");
        }
        let server = &json["$defs"]["ServerSection"]["properties"];
        for key in ["tcp_listen", "tcp_token", "http_listen", "exports"] {
            assert!(server[key].is_object(), "server.{key} missing: {server}");
        }
    }
}
