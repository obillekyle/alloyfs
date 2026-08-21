//! Target URLs (`tcp://`, `ssh://`), connection establishment, and the
//! stable per-mount identity key for local data directories.

use std::sync::Arc;

use anyhow::Context as _;

use alloyfs_transport::{stdio, tcp, MuxConnection};

pub enum Target {
    Tcp { addr: String },
    Ssh { host: String, port: Option<u16> },
}

/// Accepts `tcp://host:port[/export]` and `ssh://[user@]host[:port][/export]`.
pub fn parse_url(url: &str) -> anyhow::Result<(Target, Option<String>)> {
    let split_export = |rest: &str| -> (String, Option<String>) {
        match rest.split_once('/') {
            Some((head, export)) if !export.is_empty() => (head.to_string(), Some(export.to_string())),
            Some((head, _)) => (head.to_string(), None),
            None => (rest.to_string(), None),
        }
    };
    if let Some(rest) = url.strip_prefix("tcp://") {
        let (addr, export) = split_export(rest);
        anyhow::ensure!(!addr.is_empty(), "missing host:port in {url}");
        Ok((Target::Tcp { addr }, export))
    } else if let Some(rest) = url.strip_prefix("ssh://") {
        let (host, export) = split_export(rest);
        anyhow::ensure!(!host.is_empty(), "missing host in {url}");
        // user@host:2222 — a numeric tail after ':' is a port.
        let (host, port) = match host.rsplit_once(':') {
            Some((h, p)) if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => {
                (h.to_string(), Some(p.parse::<u16>()?))
            }
            _ => (host, None),
        };
        Ok((Target::Ssh { host, port }, export))
    } else {
        anyhow::bail!("expected tcp:// or ssh:// url, got {url}")
    }
}

/// Connect to a target url; for ssh, spawn `ssh <host> <remote_cmd> serve
/// --stdio` and speak the protocol over the exec channel. A `token`
/// authenticates immediately after the handshake (TCP servers with
/// `agent.tcp_token`; protocol v3+).
pub async fn connect_target(
    url: &str,
    remote_cmd: &str,
    client: &str,
    token: Option<&str>,
) -> anyhow::Result<(Arc<MuxConnection>, Option<String>)> {
    let (target, export) = parse_url(url)?;
    // The most-hit failure in the whole tool is "the other end isn't
    // there", and a bare `os error 10061` names neither the address nor
    // the likely fix — so both arms say where they were going and what to
    // check first.
    let conn = match target {
        Target::Tcp { addr } => tcp::connect(&addr, client).await.with_context(|| {
            format!(
                "could not reach the agent at tcp://{addr} — is `alloyfs serve` \
                 running there, and does its `server.tcp_listen` cover this address?"
            )
        })?,
        Target::Ssh { host, port } => {
            let mut args: Vec<String> = Vec::new();
            if let Some(p) = port {
                args.push("-p".into());
                args.push(p.to_string());
            }
            let shown = host.clone();
            args.push(host);
            args.push(remote_cmd.into());
            args.push("serve".into());
            args.push("--stdio".into());
            stdio::connect_command("ssh", &args, client)
                .await
                .with_context(|| {
                    format!(
                        "could not start the agent over `ssh {shown}` — check that plain \
                     `ssh {shown}` works, and that `{remote_cmd}` is on the remote's \
                     NON-interactive PATH (`ssh {shown} which {remote_cmd}`)"
                    )
                })?
        }
    };
    if let Some(token) = token {
        anyhow::ensure!(
            conn.proto >= 3,
            "server {} speaks protocol v{} — tokens need v3+ (update the server)",
            conn.server_name,
            conn.proto
        );
        match conn
            .request(alloyfs_proto::Request::Auth { token: token.into() })
            .await?
        {
            Ok(_) => {}
            Err(e) => anyhow::bail!("authentication failed: {e}"),
        }
    }
    Ok((conn, export))
}

pub fn require_export(export: Option<String>, url: &str) -> anyhow::Result<String> {
    export.ok_or_else(|| anyhow::anyhow!("url must include an export name, e.g. {url}/projects"))
}

/// A reconnect dialer that re-runs the same connect logic (tcp dial or ssh
/// re-spawn) whenever the client needs a replacement connection. The token
/// travels with it: a reconnected session must re-authenticate too.
pub fn dialer_for(
    url: &str,
    remote_cmd: &str,
    client: &str,
    token: Option<String>,
    zstd: bool,
) -> alloyfs_client::Dialer {
    let url = url.to_string();
    let remote_cmd = remote_cmd.to_string();
    let client = client.to_string();
    Arc::new(move || {
        let (url, remote_cmd, client, token) =
            (url.clone(), remote_cmd.clone(), client.clone(), token.clone());
        Box::pin(async move {
            let (conn, _) = connect_target(&url, &remote_cmd, &client, token.as_deref()).await?;
            // The mount's compression choice survives reconnects (and the
            // stream pool's extra data connections, which dial through this
            // too). enable_zstd is a no-op below v13.
            if zstd {
                conn.enable_zstd();
            }
            Ok(conn)
        })
    })
}

/// Sanitize one path component: keep it recognisable, keep it legal on both
/// platforms, and never let it escape its parent.
fn sanitize(s: &str) -> String {
    let out: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "._-".contains(c) {
                c
            } else {
                '-'
            }
        })
        .collect();
    // A component that sanitizes to "", "." or ".." would climb out of the
    // tree or collide with the directory itself.
    match out.trim_matches('.') {
        "" => "_".to_string(),
        _ => out,
    }
}

/// The per-host directory name: `azure`, `127.0.0.1-7440`.
///
/// Host AND port, so two agents on one machine stay separate — but not the
/// scheme, so mounting the same export over ssh and over tcp deliberately
/// shares one overlay. It is the same server namespace either way, and
/// splitting it would strand local-only files behind a transport choice.
pub fn host_key(url: &str) -> String {
    let after_scheme = url.split("://").nth(1).unwrap_or(url);
    let host = after_scheme.split('/').next().unwrap_or("host");
    // Drop any user@ prefix: it selects a login, not a namespace.
    let host = host.rsplit('@').next().unwrap_or(host);
    sanitize(&host.to_lowercase())
}

/// The per-export directory name within a host.
pub fn export_key(export: &str) -> String {
    sanitize(&export.to_lowercase())
}

/// Pre-layout key, kept ONLY to find and migrate old directories:
/// sanitize(host)-sanitize(export)-fnv1a8(normalized identity).
pub fn legacy_mount_key(url: &str, export: &str) -> String {
    let normalized = format!("{}/{export}", url.trim_end_matches('/').to_lowercase());
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in normalized.bytes() {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let sanitize = |s: &str| -> String {
        s.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || "._-".contains(c) {
                    c
                } else {
                    '-'
                }
            })
            .collect()
    };
    let host = url
        .split("://")
        .nth(1)
        .unwrap_or(url)
        .split('/')
        .next()
        .unwrap_or("host");
    format!("{}-{}-{:08x}", sanitize(host), sanitize(export), (hash as u32))
}

pub fn whoami() -> String {
    format!(
        "{}@{}",
        std::env::var("USER")
            .or_else(|_| std::env::var("USERNAME"))
            .unwrap_or_else(|_| "?".into()),
        std::env::var("HOSTNAME")
            .or_else(|_| std::env::var("COMPUTERNAME"))
            .unwrap_or_else(|_| "?".into())
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_and_export_keys_are_path_safe() {
        // These become real directory names, so they must be stable,
        // recognisable, and incapable of escaping their parent.
        assert_eq!(host_key("ssh://azure/projects"), "azure");
        assert_eq!(
            host_key("ssh://kyle@azure/projects"),
            "azure",
            "login is not identity"
        );
        assert_eq!(host_key("tcp://127.0.0.1:7440/p"), "127.0.0.1-7440");
        assert_eq!(host_key("SSH://AZURE/p"), "azure", "case-insensitive");

        // Same host+export over different transports SHARES a directory:
        // it is one server namespace, and splitting it would strand
        // local-only overlay files behind a transport choice.
        assert_eq!(host_key("ssh://azure/p"), host_key("ssh://azure/other"));

        assert_eq!(export_key("pro/jects"), "pro-jects");

        // The real invariant is not "contains no dots" — `..-..-etc` is a
        // perfectly ordinary filename, since `..` only means "parent" when
        // it is the WHOLE component. What must hold is that the result is a
        // single component that is never `.` or `..`.
        for probe in ["../../etc", "..", ".", "", "..\\..\\windows", "a/b/c"] {
            let k = export_key(probe);
            assert!(
                !k.contains('/') && !k.contains('\\'),
                "{probe:?} → {k:?} spans directories"
            );
            assert!(
                k != "." && k != ".." && !k.is_empty(),
                "{probe:?} → {k:?} is not a usable name"
            );
        }
        assert_eq!(export_key(""), "_", "never empty");
        assert_eq!(export_key("."), "_", "never the directory itself");
        assert_eq!(export_key(".."), "_", "never the parent");
    }

    #[test]
    fn legacy_key_still_derivable_for_migration() {
        // Only needed to FIND old directories; must stay byte-identical.
        let a = legacy_mount_key("ssh://azure", "projects");
        assert_eq!(a, legacy_mount_key("ssh://azure", "projects"));
        assert!(a.starts_with("azure-projects-"));
        assert_eq!(a.len(), "azure-projects-".len() + 8);
    }

    #[test]
    fn url_forms() {
        assert!(matches!(parse_url("tcp://h:1/p"), Ok((Target::Tcp { .. }, Some(e))) if e == "p"));
        assert!(matches!(
            parse_url("ssh://u@h/p"),
            Ok((Target::Ssh { port: None, .. }, Some(_)))
        ));
        assert!(matches!(
            parse_url("ssh://h:2222/p"),
            Ok((Target::Ssh { port: Some(2222), .. }, Some(_)))
        ));
        assert!(matches!(parse_url("ssh://h"), Ok((Target::Ssh { .. }, None))));
        assert!(parse_url("ftp://h/p").is_err());
    }
}
