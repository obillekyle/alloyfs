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

/// Restart backoff for a unit that died: doubling from here...
const RESTART_MIN: Duration = Duration::from_secs(1);
/// ...to at most this, so a permanently broken unit costs one line a
/// half-minute instead of a spin.
const RESTART_MAX: Duration = Duration::from_secs(30);
/// A unit that stayed up this long before dying is a fresh incident rather
/// than a crash loop, so its next restart starts the ladder over.
const STABLE_AFTER: Duration = Duration::from_secs(60);

/// One restartable thing: the agent, or a mount. A factory rather than a
/// future, because a supervisor has to be able to start it again.
type Unit = Box<
    dyn Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send>> + Send + Sync,
>;

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

    let mut tasks: Vec<(String, tokio::task::JoinHandle<()>)> = Vec::new();

    if serve_wanted {
        let listen = agent_listen_addr(&cfg);
        let cfg_path = config.clone();
        let unit: Unit = Box::new(move || {
            let cfg_path = cfg_path.clone();
            Box::pin(async move { super::serve::run(None, false, cfg_path, Vec::new()).await })
        });
        tasks.push((
            "agent".to_string(),
            tokio::spawn(supervise("agent".to_string(), unit)),
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
        // A `cache:` block supplies all three; the flat keys still work where
        // it is absent. Same precedence as `mount`, so a config behaves the
        // same whether it is started by name or by `alloyfs start`.
        let (c_size, c_max, c_warm) = crate::config::CacheConfig::resolve(mount.cache.as_ref())
            .map_err(|e| anyhow::anyhow!("mount {label}: {e}"))?;
        let stated_cache = mount.cache.is_some();
        let cache_max = if stated_cache {
            Some(c_size.to_string())
        } else {
            mount
                .auto_cache_max
                .clone()
                .map(|s| s.to_bytes())
                .transpose()
                .map_err(|e| anyhow::anyhow!("mount {label}: auto_cache_max: {e}"))?
                .map(|b| b.to_string())
        };
        let cache_budget = if stated_cache {
            Some(c_max.to_string())
        } else {
            mount
                .auto_cache_budget
                .clone()
                .map(|s| s.to_bytes())
                .transpose()
                .map_err(|e| anyhow::anyhow!("mount {label}: auto_cache_budget: {e}"))?
                .map(|b| b.to_string())
        };
        let cache_warm = stated_cache.then(|| c_warm.to_string());
        let unit: Unit = Box::new(move || {
            // Cloned per attempt: a restart must hand `mount::run` the same
            // arguments the first try got.
            let mount = mount.clone();
            let (cache_max, cache_budget, cache_warm) =
                (cache_max.clone(), cache_budget.clone(), cache_warm.clone());
            Box::pin(async move {
                super::mount::run(
                    mount.url,
                    mount.at,
                    "alloyfs".to_string(),
                    None,
                    mount.exclude,
                    mount.pin,
                    cache_max,
                    cache_budget,
                    cache_warm,
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
            })
        });
        tasks.push((name, tokio::spawn(supervise(label.clone(), unit))));
        println!("mounting {label}");
    }

    // Ctrl-C, or every unit having finished for good. Neither ends the
    // others: an unreachable host must not take the agent down, and a failed
    // mount must not take the working ones with it.
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            println!();
            println!("stopping.");
        }
        () = watch_for_exits(&mut tasks) => {}
    }

    // What reaches here is a supervisor that PANICKED — ordinary failures
    // are logged and retried by `supervise` instead of waiting silently for
    // the last unit to finish. The exit code still says something went
    // wrong, and the output still says which one, because "start failed"
    // about a five-mount config is not a useful thing to be told.
    let mut failed = Vec::new();
    for (name, handle) in tasks {
        if handle.is_finished() {
            match handle.await {
                Ok(()) => {}
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

/// Keep one unit running for as long as this command does.
///
/// A returning unit is either done (`Ok` — unmounted, or the process is
/// stopping) or dead (`Err`), and dead is the case that used to vanish:
/// failures were collected only after EVERY task had finished, so one dead
/// mount out of three printed nothing, was never retried, and left the
/// process alive — which also meant systemd's `Restart=on-failure` never
/// fired, because nothing ever exited. The drive was simply absent, with no
/// trace anywhere. Now the death is logged where it happens and the unit
/// comes back on a backoff ladder.
///
/// No attempt cap, deliberately: the outage this exists to survive is a
/// server that is down for a few minutes, and a cap would give up on
/// precisely that. The DELAY is capped instead.
async fn supervise(name: String, run: Unit) {
    let mut delay = RESTART_MIN;
    let mut attempt = 0u32;
    loop {
        let started = tokio::time::Instant::now();
        match run().await {
            Ok(()) => return,
            Err(e) => {
                let ran_for = started.elapsed();
                if ran_for >= STABLE_AFTER {
                    delay = RESTART_MIN;
                    attempt = 0;
                }
                attempt += 1;
                tracing::error!(
                    unit = %name,
                    error = %e,
                    ran_for_s = ran_for.as_secs(),
                    attempt,
                    retry_in_s = delay.as_secs(),
                    "unit stopped; restarting"
                );
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(RESTART_MAX);
            }
        }
    }
}

/// Resolve after every task has either finished or been cancelled.
///
/// Returning on the FIRST failure would defeat the point: the remaining mounts
/// are still working and their owner still wants them.
async fn watch_for_exits(tasks: &mut [(String, tokio::task::JoinHandle<()>)]) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    /// A unit that keeps failing is restarted, not abandoned — and the
    /// wait between tries grows instead of spinning. Time is paused, so
    /// this asserts the ladder rather than the wall clock.
    #[tokio::test(start_paused = true)]
    async fn a_failing_unit_is_restarted_with_growing_delay() {
        let attempts = Arc::new(AtomicU32::new(0));
        let seen = attempts.clone();
        let unit: Unit = Box::new(move || {
            let seen = seen.clone();
            Box::pin(async move {
                seen.fetch_add(1, Ordering::Relaxed);
                anyhow::bail!("nope")
            })
        });
        let task = tokio::spawn(supervise("t".into(), unit));

        // 1s + 2s + 4s + 8s of backoff covers five attempts and not a sixth
        // (the next one waits until t=16s).
        tokio::time::sleep(Duration::from_millis(15_500)).await;
        assert_eq!(
            attempts.load(Ordering::Relaxed),
            5,
            "the ladder must double, not spin"
        );
        task.abort();
    }

    /// A unit that finishes cleanly is done: the supervisor returns rather
    /// than restarting something nobody asked to keep.
    #[tokio::test(start_paused = true)]
    async fn a_clean_exit_is_not_restarted() {
        let attempts = Arc::new(AtomicU32::new(0));
        let seen = attempts.clone();
        let unit: Unit = Box::new(move || {
            let seen = seen.clone();
            Box::pin(async move {
                seen.fetch_add(1, Ordering::Relaxed);
                Ok(())
            })
        });
        supervise("t".into(), unit).await;
        assert_eq!(attempts.load(Ordering::Relaxed), 1);
    }
}
