//! `alloyfs start` — everything one config describes, in one command.
//!
//! The agent from `server:`, then every mount under `client.mounts`, running
//! together until Ctrl-C. It exists because a v3 config describes both halves
//! of a machine and starting them separately means two terminals and a
//! remembered order.

use std::path::PathBuf;
use std::time::Duration;

use crate::config::{Config, ResolvedMount};

/// How long to wait for our own agent to start listening before giving up and
/// mounting anyway.
const AGENT_READY_TIMEOUT: Duration = Duration::from_secs(10);

pub async fn run(config: Option<PathBuf>, server_only: bool, mounts_only: bool) -> anyhow::Result<()> {
    let cfg = crate::config::load_or_default(config.clone())?;

    let serve_wanted = !mounts_only && cfg.has_exports();
    let mounts: Vec<(String, ResolvedMount)> = if server_only {
        Vec::new()
    } else {
        cfg.client
            .as_ref()
            .map(|c| c.resolved_mounts())
            .unwrap_or_default()
    };

    // Nothing configured is not a failure. A machine that has not been set up
    // yet should be told so, not handed an error to interpret.
    if !serve_wanted && mounts.is_empty() {
        println!("nothing to start.");
        println!();
        if server_only {
            println!("  --server-only was given, but the config defines no exports.");
        } else if mounts_only {
            println!("  --mounts-only was given, but the config defines no mounts.");
        } else {
            println!("  Add exports under `server:` or mounts under `client.mounts:`.");
            println!("  `alloyfs init` writes a starting point.");
        }
        return Ok(());
    }

    let mut tasks: Vec<(String, tokio::task::JoinHandle<anyhow::Result<()>>)> = Vec::new();

    if serve_wanted {
        let listen = agent_listen_addr(&cfg);
        let cfg_path = config.clone();
        tasks.push((
            "agent".to_string(),
            tokio::spawn(async move { super::serve::run(None, false, cfg_path, Vec::new()).await }),
        ));
        println!("serving {} export(s) on {listen}", export_count(&cfg));

        // Wait for the socket before mounting. A config whose own
        // `client.mounts` points back at its own `server:` is an ordinary
        // thing to write — a loopback mount of a local export — and without
        // this the mount races the listener and loses.
        if !mounts.is_empty() {
            wait_until_listening(&listen).await;
        }
    }

    for (name, mount) in mounts {
        let label = name.clone();
        // Sizes travel as a plain byte count: `mount::run` parses whatever it
        // is given, and a resolved `2M` and a resolved `2097152` must reach it
        // as the same number rather than as two spellings.
        let cache_max = mount
            .auto_cache_max
            .map(|s| s.to_bytes())
            .transpose()
            .map_err(|e| anyhow::anyhow!("mount {label}: auto_cache_max: {e}"))?
            .map(|b| b.to_string());
        let cache_budget = mount
            .auto_cache_budget
            .map(|s| s.to_bytes())
            .transpose()
            .map_err(|e| anyhow::anyhow!("mount {label}: auto_cache_budget: {e}"))?
            .map(|b| b.to_string());
        tasks.push((
            name,
            tokio::spawn(async move {
                super::mount::run(
                    mount.url,
                    mount.at,
                    "alloyfs".to_string(),
                    None,
                    mount.exclude,
                    mount.pin,
                    cache_max,
                    cache_budget,
                    mount.data_dir,
                    mount.no_server_defaults,
                    // Config-driven mounts keep the batched default; the
                    // opt-out is a CLI decision until a config key earns it.
                    false,
                    mount.zstd,
                    mount.detect_conflicts,
                    mount.token,
                    super::mount::Backend::default(),
                )
                .await
            }),
        ));
        println!("mounting {label}");
    }

    // Ctrl-C, or the first task to fall over. Neither ends the others: an
    // unreachable host must not take the agent down, and a failed mount must
    // not take the working ones with it.
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            println!();
            println!("stopping.");
        }
        () = watch_for_exits(&mut tasks) => {}
    }

    // Collect what failed. The exit code says something went wrong; the output
    // says which one, because "start failed" about a five-mount config is not
    // a useful thing to be told.
    let mut failed = Vec::new();
    for (name, handle) in tasks {
        if handle.is_finished() {
            match handle.await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => failed.push(format!("{name}: {e}")),
                Err(e) if e.is_cancelled() => {}
                Err(e) => failed.push(format!("{name}: {e}")),
            }
        } else {
            handle.abort();
        }
    }
    anyhow::ensure!(failed.is_empty(), "{}", failed.join("\n"));
    Ok(())
}

/// Resolve after every task has either finished or been cancelled.
///
/// Returning on the FIRST failure would defeat the point: the remaining mounts
/// are still working and their owner still wants them.
async fn watch_for_exits(tasks: &mut [(String, tokio::task::JoinHandle<anyhow::Result<()>>)]) {
    loop {
        if tasks.iter().all(|(_, h)| h.is_finished()) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn export_count(cfg: &Config) -> usize {
    cfg.server
        .as_ref()
        .and_then(|s| s.exports.as_ref())
        .map(|e| e.len())
        .unwrap_or(0)
}

fn agent_listen_addr(cfg: &Config) -> String {
    cfg.server
        .as_ref()
        .and_then(|s| s.tcp_listen.clone())
        .unwrap_or_else(|| "127.0.0.1:7440".to_string())
}

/// Poll the agent's own socket until it accepts.
///
/// A poll rather than a readiness channel because `serve::run` blocks inside
/// `tcp::serve` for the life of the process; the socket is the only signal it
/// offers, and it is the one the mounts care about anyway.
///
/// A `0.0.0.0` listen is dialled on loopback: the wildcard is what to bind,
/// not an address to connect to.
async fn wait_until_listening(listen: &str) {
    let target = listen
        .replace("0.0.0.0:", "127.0.0.1:")
        .replace("[::]:", "[::1]:");
    let deadline = tokio::time::Instant::now() + AGENT_READY_TIMEOUT;
    while tokio::time::Instant::now() < deadline {
        if tokio::net::TcpStream::connect(&target).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Not fatal. The mounts may be pointed somewhere else entirely, in which
    // case our agent's readiness was never their problem.
    tracing::warn!(
        listen,
        "the local agent is not accepting connections yet; mounting anyway"
    );
}
