use std::path::PathBuf;
use std::sync::Arc;

use alloyfs_agent::{AgentSession, ExportRegistry};
use alloyfs_transport::{stdio, tcp, RequestHandler};

use crate::config::load_agent_config;

pub async fn run(
    addr: Option<String>,
    stdio_mode: bool,
    config: Option<PathBuf>,
    exports: Vec<String>,
) -> anyhow::Result<()> {
    let cfg = load_agent_config(config, &exports)?;
    let registry = Arc::new(ExportRegistry::from_config(&cfg)?);
    // One watcher per export; guards keep the OS watchers alive for the life
    // of the process.
    let mut _watch_guards = Vec::new();
    for export in registry.all() {
        let hub = export.events.clone();
        let name = export.name.clone();
        // A failed watcher degrades that export to no-events; it must never
        // take the whole agent down.
        match alloyfs_agent::watch::spawn(export, hub, std::time::Duration::from_millis(250)) {
            Ok(guard) => _watch_guards.push(guard),
            Err(e) => tracing::warn!(export = name, error = %e, "file watching disabled"),
        }
    }
    // Frees locks/handles of clients that vanish without disconnecting
    // (heartbeats arrive every 10 s; 30 s of silence = dead).
    registry.spawn_lease_reaper(std::time::Duration::from_secs(30));

    let name = format!("alloyfs/{}", env!("CARGO_PKG_VERSION"));
    if stdio_mode {
        // One session over our own stdin/stdout (the ssh exec channel).
        // stdout carries protocol frames; logging is stderr-only. No tcp_token
        // check: reaching this process already required an ssh login.
        //
        // Note what is NOT started here: the HTTP API. A --stdio agent is a
        // short-lived per-mount process spawned by ssh, and every one of them
        // tried to bind the configured HTTP port — so each mount logged a
        // bind failure once a resident agent held it. The API belongs to the
        // long-running server, not to a mount's transport.
        stdio::serve(&name, Arc::new(AgentSession::new(registry)), cfg.agent.zstd).await?;
    } else {
        if let Some(http) = cfg.agent.http_listen.clone() {
            let registry = registry.clone();
            let token = cfg.agent.http_token.clone();
            tokio::spawn(async move {
                if let Err(e) = alloyfs_http::serve(&http, registry, token).await {
                    tracing::error!(error = %e, "http api failed");
                }
            });
        }
        // Documented precedence: CLI flag, then config, then the built-in
        // default. `--tcp` used to lose to the config because clap always
        // supplied a value, so an explicit flag was silently ignored.
        let listen = addr
            .or(cfg.agent.tcp_listen)
            .unwrap_or_else(|| "127.0.0.1:7440".to_string());
        let token = cfg.agent.tcp_token.clone();
        if token.is_none() && !alloyfs_common::is_loopback_listen(&listen) {
            anyhow::bail!(
                "refusing to serve TCP on non-loopback {listen} without agent.tcp_token — \
                 anyone who can reach the port could mount every export"
            );
        }
        // Token-protected listeners require proto v3: older clients can't
        // decode AuthRequired, so they're turned away at the handshake.
        let min_proto = if token.is_some() {
            3
        } else {
            alloyfs_proto::PROTO_VERSION_MIN
        };
        tcp::serve(&listen, name, min_proto, cfg.agent.zstd, move || {
            Arc::new(AgentSession::with_token(registry.clone(), token.clone())) as Arc<dyn RequestHandler>
        })
        .await?;
    }
    Ok(())
}
