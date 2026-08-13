//! Target URLs (`tcp://`, `ssh://`), connection establishment, and the
//! stable per-mount identity key for local data directories.

use std::sync::Arc;

use ds_transport::{stdio, tcp, MuxConnection};

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
/// --stdio` and speak the protocol over the exec channel.
pub async fn connect_target(
    url: &str,
    remote_cmd: &str,
    client: &str,
) -> anyhow::Result<(Arc<MuxConnection>, Option<String>)> {
    let (target, export) = parse_url(url)?;
    let conn = match target {
        Target::Tcp { addr } => tcp::connect(&addr, client).await?,
        Target::Ssh { host, port } => {
            let mut args: Vec<String> = Vec::new();
            if let Some(p) = port {
                args.push("-p".into());
                args.push(p.to_string());
            }
            args.push(host);
            args.push(remote_cmd.into());
            args.push("serve".into());
            args.push("--stdio".into());
            stdio::connect_command("ssh", &args, client).await?
        }
    };
    Ok((conn, export))
}

pub fn require_export(export: Option<String>, url: &str) -> anyhow::Result<String> {
    export.ok_or_else(|| anyhow::anyhow!("url must include an export name, e.g. {url}/projects"))
}

/// A reconnect dialer that re-runs the same connect logic (tcp dial or ssh
/// re-spawn) whenever the client needs a replacement connection.
pub fn dialer_for(url: &str, remote_cmd: &str, client: &str) -> ds_client::Dialer {
    let url = url.to_string();
    let remote_cmd = remote_cmd.to_string();
    let client = client.to_string();
    Arc::new(move || {
        let (url, remote_cmd, client) = (url.clone(), remote_cmd.clone(), client.clone());
        Box::pin(async move {
            let (conn, _) = connect_target(&url, &remote_cmd, &client).await?;
            Ok(conn)
        })
    })
}

/// Stable per-(server, export) key for local data dirs:
/// sanitize(host)-sanitize(export)-fnv1a8(normalized identity).
pub fn mount_key(url: &str, export: &str) -> String {
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
    fn mount_key_is_stable_and_sanitized() {
        let a = mount_key("ssh://azure", "projects");
        let b = mount_key("ssh://azure", "projects");
        assert_eq!(a, b, "same inputs, same key");
        assert!(a.starts_with("azure-projects-"));
        assert_eq!(a.len(), "azure-projects-".len() + 8);
        let c = mount_key("ssh://azure:2222", "projects");
        assert_ne!(a, c, "port changes identity");
        let d = mount_key("tcp://127.0.0.1:7440", "pro/jects");
        assert!(!d.contains('/'), "sanitized: {d}");
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
