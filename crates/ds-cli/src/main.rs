use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use clap::{Parser, Subcommand};
use ds_agent::{AgentConfig, AgentSession, ExportRegistry};
use ds_proto::{Request, Response};
use ds_transport::{tcp, RequestHandler};

#[derive(Parser)]
#[command(name = "drive-sync", version, about = "Cross-platform virtual drive service")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the agent, serving exports to clients.
    Serve {
        /// TCP listen address, e.g. 127.0.0.1:7440
        #[arg(long, default_value = "127.0.0.1:7440")]
        tcp: String,
        /// Serve one session over stdin/stdout instead of TCP (what
        /// `mount ssh://...` runs on the remote side).
        #[arg(long)]
        stdio: bool,
        /// TOML config file with [exports.*] sections.
        #[arg(long)]
        config: Option<PathBuf>,
        /// Inline export, NAME=PATH (repeatable). Alternative to --config.
        #[arg(long = "export", value_name = "NAME=PATH")]
        exports: Vec<String>,
    },
    /// Mount an export as a local drive.
    /// URL: tcp://host:port/export or ssh://host/export
    Mount {
        url: String,
        /// Mountpoint: a directory on Linux, a drive letter (X:) on Windows.
        mountpoint: PathBuf,
        /// Remote drive-sync command for ssh:// urls.
        #[arg(long, default_value = "drive-sync")]
        remote_cmd: String,
        /// YAML mount config (exclude/pin/auto_cache_max/auto_cache_budget/
        /// data_dir). CLI flags override file values.
        #[arg(long)]
        config: Option<PathBuf>,
        /// Local-only paths (gitignore-style glob, repeatable): stored on this
        /// machine, never sent to the server. e.g. --exclude node_modules
        #[arg(long = "exclude", value_name = "GLOB")]
        excludes: Vec<String>,
        /// Always fully cache matching files regardless of size (repeatable).
        #[arg(long = "pin", value_name = "GLOB")]
        pins: Vec<String>,
        /// Auto-download files up to this size ("2M", "512K", bytes; 0 = off).
        #[arg(long, value_name = "SIZE")]
        auto_cache_max: Option<String>,
        /// Total local cache budget (LRU eviction; pins exempt).
        #[arg(long, value_name = "SIZE")]
        auto_cache_budget: Option<String>,
        /// Local data directory (overlay + cache). Default: per-user app data.
        #[arg(long, value_name = "PATH")]
        data_dir: Option<PathBuf>,
    },
    /// Manage the local auto-download cache.
    Cache {
        #[command(subcommand)]
        cmd: CacheCmd,
    },
    /// Tail an export's live change events as NDJSON
    /// (url: tcp://host:port/export or ssh://host/export).
    Events {
        url: String,
        /// Resume from this sequence number (catch-up from the server's ring log).
        #[arg(long)]
        since: Option<u64>,
        /// Remote drive-sync command for ssh:// urls.
        #[arg(long, default_value = "drive-sync")]
        remote_cmd: String,
    },
    /// Measure round-trip latency to an agent (url: tcp://host:port or ssh://host).
    Ping {
        url: String,
        #[arg(long, default_value_t = 5)]
        count: u32,
    },
    /// Fire many concurrent pipelined requests at an agent and verify replies.
    Stress {
        url: String,
        #[arg(long, default_value_t = 1000)]
        count: u32,
    },
}

#[derive(clap::Subcommand)]
enum CacheCmd {
    /// Delete cached blobs for one mount url (SAFE while unmounted). The
    /// overlay (local-only excluded files) is NEVER touched — deleting that
    /// would lose the only copy.
    Clear {
        /// Mount url incl. export (e.g. ssh://azure/projects), or --all.
        target: Option<String>,
        #[arg(long)]
        all: bool,
        #[arg(long, value_name = "PATH")]
        data_dir: Option<PathBuf>,
    },
}

/// Per-mount client config file (YAML). All keys optional; CLI flags win.
#[derive(Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct MountConfig {
    #[serde(default)]
    exclude: Vec<String>,
    #[serde(default)]
    pin: Vec<String>,
    auto_cache_max: Option<SizeField>,
    auto_cache_budget: Option<SizeField>,
    data_dir: Option<PathBuf>,
}

/// Accepts `auto_cache_max: 2M` (string) or `auto_cache_max: 2097152` (int).
#[derive(serde::Deserialize)]
#[serde(untagged)]
enum SizeField {
    Bytes(u64),
    Human(String),
}

impl SizeField {
    fn to_bytes(&self) -> Result<u64, String> {
        match self {
            SizeField::Bytes(n) => Ok(*n),
            SizeField::Human(s) => parse_size(s),
        }
    }
}

/// "2M" → 2 MiB, "512K", "1G", bare digits = bytes, "0" disables.
fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty size".into());
    }
    let (digits, mult) = match s.chars().last().unwrap().to_ascii_uppercase() {
        'K' => (&s[..s.len() - 1], 1024u64),
        'M' => (&s[..s.len() - 1], 1024 * 1024),
        'G' => (&s[..s.len() - 1], 1024 * 1024 * 1024),
        c if c.is_ascii_digit() => (s, 1),
        c => return Err(format!("unknown size suffix {c:?} in {s:?}")),
    };
    digits
        .parse::<u64>()
        .map_err(|e| format!("bad size {s:?}: {e}"))?
        .checked_mul(mult)
        .ok_or_else(|| format!("size {s:?} overflows"))
}

/// Stable per-(server, export) key for local data dirs:
/// sanitize(host)-sanitize(export)-fnv1a8(normalized identity).
fn mount_key(url: &str, export: &str) -> String {
    let normalized = format!("{}/{export}", url.trim_end_matches('/').to_lowercase());
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in normalized.bytes() {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let sanitize = |s: &str| -> String {
        s.chars()
            .map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '-' })
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

fn default_data_dir() -> PathBuf {
    #[cfg(windows)]
    {
        std::env::var("LOCALAPPDATA")
            .map(PathBuf::from)
            .or_else(|_| std::env::var("USERPROFILE").map(|p| PathBuf::from(p).join("AppData/Local")))
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("drive-sync")
    }
    #[cfg(unix)]
    {
        std::env::var("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".local/share")))
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("drive-sync")
    }
}

enum Target {
    Tcp { addr: String },
    Ssh { host: String, port: Option<u16> },
}

/// Accepts `tcp://host:port[/export]` and `ssh://[user@]host[:port][/export]`.
fn parse_url(url: &str) -> anyhow::Result<(Target, Option<String>)> {
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
async fn connect_target(
    url: &str,
    remote_cmd: &str,
    client: &str,
) -> anyhow::Result<(Arc<ds_transport::MuxConnection>, Option<String>)> {
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
            ds_transport::stdio::connect_command("ssh", &args, client).await?
        }
    };
    Ok((conn, export))
}

fn require_export(export: Option<String>, url: &str) -> anyhow::Result<String> {
    export.ok_or_else(|| anyhow::anyhow!("url must include an export name, e.g. {url}/projects"))
}

/// Config search order when --config/--export are absent (matters for
/// `serve --stdio`, which is spawned remotely with no arguments).
/// YAML preferred; TOML kept for existing deployments.
fn default_config_path() -> Option<PathBuf> {
    #[cfg(unix)]
    let dir = std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(".config/drive-sync"));
    #[cfg(windows)]
    let dir = Some(PathBuf::from("C:\\MyApps"));
    let dir = dir?;
    #[cfg(unix)]
    let names = ["agent.yml", "agent.yaml", "agent.toml"];
    #[cfg(windows)]
    let names = ["drive-sync.yml", "drive-sync.yaml", "drive-sync.toml"];
    names.iter().map(|n| dir.join(n)).find(|p| p.is_file())
}

fn load_agent_config(
    config: Option<PathBuf>,
    inline_exports: &[String],
) -> anyhow::Result<AgentConfig> {
    let mut cfg = match config {
        Some(path) => AgentConfig::from_path(&path)?,
        // No explicit config: a default file (if present) supplies exports —
        // essential for `serve --stdio`, which is spawned with no arguments.
        None if inline_exports.is_empty() => match default_config_path() {
            Some(path) => {
                tracing::info!(path = %path.display(), "using default config");
                AgentConfig::from_path(&path)?
            }
            None => AgentConfig::default(),
        },
        None => AgentConfig::default(),
    };
    for spec in inline_exports {
        let (name, path) = spec
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("--export wants NAME=PATH, got {spec}"))?;
        cfg.exports.insert(
            name.to_string(),
            ds_agent::ExportConfig { path: PathBuf::from(path), read_only: false, exclude: vec![] },
        );
    }
    Ok(cfg)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr) // stdout stays clean: future --stdio transport uses it
        .init();

    match Cli::parse().command {
        Command::Serve { tcp: addr, stdio, config, exports } => {
            let cfg = load_agent_config(config, &exports)?;
            let registry = Arc::new(ExportRegistry::from_config(&cfg)?);
            // One watcher per export; guards keep the OS watchers alive for
            // the life of the process.
            let mut _watch_guards = Vec::new();
            for export in registry.all() {
                let hub = export.events.clone();
                let name = export.name.clone();
                // A failed watcher degrades that export to no-events; it must
                // never take the whole agent down.
                match ds_agent::watch::spawn(export, hub, std::time::Duration::from_millis(250)) {
                    Ok(guard) => _watch_guards.push(guard),
                    Err(e) => tracing::warn!(export = name, error = %e, "file watching disabled"),
                }
            }
            // Frees locks/handles of clients that vanish without disconnecting
            // (heartbeats arrive every 10 s; 30 s of silence = dead).
            registry.spawn_lease_reaper(std::time::Duration::from_secs(30));
            if let Some(http) = cfg.agent.http_listen.clone() {
                let registry = registry.clone();
                let token = cfg.agent.http_token.clone();
                tokio::spawn(async move {
                    if let Err(e) = ds_http::serve(&http, registry, token).await {
                        tracing::error!(error = %e, "http api failed");
                    }
                });
            }
            let name = format!("drive-sync/{}", env!("CARGO_PKG_VERSION"));
            if stdio {
                // One session over our own stdin/stdout (the ssh exec channel).
                // stdout carries protocol frames; logging is stderr-only.
                ds_transport::stdio::serve(&name, Arc::new(AgentSession::new(registry)))
                    .await?;
            } else {
                let listen = cfg.agent.tcp_listen.unwrap_or(addr);
                tcp::serve(&listen, name, move || {
                    Arc::new(AgentSession::new(registry.clone())) as Arc<dyn RequestHandler>
                })
                .await?;
            }
        }
        Command::Mount {
            url,
            mountpoint,
            remote_cmd,
            config,
            excludes,
            pins,
            auto_cache_max,
            auto_cache_budget,
            data_dir,
        } => {
            // File values first, CLI flags override.
            let file_cfg: MountConfig = match &config {
                Some(path) => serde_yaml::from_str(&std::fs::read_to_string(path)?)
                    .map_err(|e| anyhow::anyhow!("mount config {}: {e}", path.display()))?,
                None => MountConfig::default(),
            };
            let excludes = if excludes.is_empty() { file_cfg.exclude } else { excludes };
            let pins = if pins.is_empty() { file_cfg.pin } else { pins };
            let auto_cache_max = match (auto_cache_max, &file_cfg.auto_cache_max) {
                (Some(flag), _) => parse_size(&flag).map_err(|e| anyhow::anyhow!(e))?,
                (None, Some(f)) => f.to_bytes().map_err(|e| anyhow::anyhow!(e))?,
                (None, None) => 2 * 1024 * 1024,
            };
            let auto_cache_budget = match (auto_cache_budget, &file_cfg.auto_cache_budget) {
                (Some(flag), _) => parse_size(&flag).map_err(|e| anyhow::anyhow!(e))?,
                (None, Some(f)) => f.to_bytes().map_err(|e| anyhow::anyhow!(e))?,
                (None, None) => 512 * 1024 * 1024,
            };
            let data_dir = data_dir.or(file_cfg.data_dir).unwrap_or_else(default_data_dir);

            let (conn, export) = connect_target(&url, &remote_cmd, &whoami()).await?;
            let export = require_export(export, &url)?;
            tracing::info!(server = conn.server_name, proto = conn.proto, "connected");
            let opts = ds_client::ClientOptions {
                mount_key: mount_key(&url, &export),
                excludes,
                pins,
                auto_cache_max,
                auto_cache_budget,
                data_dir,
            };
            let fs = ds_client::RemoteFs::attach_with(conn, &export, opts).await?;
            // Each platform starts the event pump itself, wiring server events
            // into its native notification mechanism.
            mount_platform(fs, mountpoint, &export).await?;
        }
        Command::Cache { cmd } => match cmd {
            CacheCmd::Clear { target, all, data_dir } => {
                let base = data_dir.unwrap_or_else(default_data_dir).join("cache");
                if all {
                    if base.exists() {
                        std::fs::remove_dir_all(&base)?;
                    }
                    println!("cleared all caches under {}", base.display());
                } else {
                    let url = target
                        .ok_or_else(|| anyhow::anyhow!("pass a mount url (with export) or --all"))?;
                    let (_, export) = parse_url(&url)?;
                    let export = require_export(export, &url)?;
                    let key = mount_key(&url, &export);
                    let dir = base.join(&key);
                    let manifest = base.join(format!("{key}.manifest.json"));
                    if dir.exists() {
                        std::fs::remove_dir_all(&dir)?;
                    }
                    let _ = std::fs::remove_file(&manifest);
                    println!("cleared cache for {key}");
                }
            }
        },
        Command::Events { url, since, remote_cmd } => {
            let (conn, export) = connect_target(&url, &remote_cmd, &whoami()).await?;
            let export = require_export(export, &url)?;
            let fs = ds_client::RemoteFs::attach(conn.clone(), &export).await?;
            let mut rx = conn.events();
            let last_seq = fs.start_event_pump(|_| {}).await?;
            tracing::info!(last_seq, "subscribed; streaming events (NDJSON)");
            let _ = since; // catch-up via --since arrives with reconnect support
            loop {
                match rx.recv().await {
                    Ok(batch) => {
                        for ev in batch {
                            println!("{}", serde_json::to_string(&ev)?);
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(missed = n, "event stream lagged");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        }
        Command::Ping { url, count } => {
            let (conn, _) = connect_target(&url, "drive-sync", "ping").await?;
            println!("connected to {} (proto v{})", conn.server_name, conn.proto);
            for i in 1..=count {
                let rtt = conn.ping().await?;
                println!("ping {i}: {:.3} ms", rtt.as_secs_f64() * 1000.0);
            }
        }
        Command::Stress { url, count } => {
            let (conn, _) = connect_target(&url, "drive-sync", "stress").await?;
            let start = Instant::now();
            // Launch every request before awaiting any: true pipelining.
            let mut futs = Vec::with_capacity(count as usize);
            for _ in 0..count {
                let conn = conn.clone();
                futs.push(tokio::spawn(async move { conn.request(Request::Statfs).await }));
            }
            let mut ok = 0u32;
            for f in futs {
                match f.await? {
                    Ok(Ok(Response::Statfs { .. })) => ok += 1,
                    other => anyhow::bail!("unexpected reply: {other:?}"),
                }
            }
            let dt = start.elapsed();
            println!(
                "{ok}/{count} pipelined requests OK in {:.1} ms ({:.0} req/s)",
                dt.as_secs_f64() * 1000.0,
                count as f64 / dt.as_secs_f64()
            );
        }
    }
    Ok(())
}

fn whoami() -> String {
    format!("{}@{}", std::env::var("USER").or_else(|_| std::env::var("USERNAME")).unwrap_or_else(|_| "?".into()),
        std::env::var("HOSTNAME").or_else(|_| std::env::var("COMPUTERNAME")).unwrap_or_else(|_| "?".into()))
}

#[cfg(unix)]
async fn mount_platform(
    fs: Arc<ds_client::RemoteFs>,
    mountpoint: PathBuf,
    export: &str,
) -> anyhow::Result<()> {
    let export = export.to_string();
    let (notifier_tx, notifier_rx) = tokio::sync::oneshot::channel();
    let mount_fs = fs.clone();
    // The mount blocks its thread until unmount; the runtime stays free to
    // service the connection and the event pump.
    let mount_task = tokio::task::spawn_blocking(move || {
        ds_mount_fuse::mount(mount_fs, &mountpoint, &export, move |notifier| {
            let _ = notifier_tx.send(notifier);
        })
    });
    if let Ok(notifier) = notifier_rx.await {
        // Server events → cache invalidation (inside the pump) → kernel cache
        // eviction, so the next stat/read/listing is instantly fresh.
        let pump_fs = fs.clone();
        fs.start_event_pump(move |batch| {
            ds_mount_fuse::apply_events_native(&pump_fs, &notifier, batch);
        })
        .await?;
    }
    mount_task.await??;
    fs.shutdown(); // persist the auto-cache manifest
    Ok(())
}

#[cfg(windows)]
async fn mount_platform(
    fs: Arc<ds_client::RemoteFs>,
    mountpoint: PathBuf,
    export: &str,
) -> anyhow::Result<()> {
    let mountpoint = mountpoint.to_string_lossy().into_owned();
    // mount() returns once the WinFsp dispatcher is running; its threads call
    // back into RemoteFs, which block_on's this runtime — so we keep the
    // runtime alive here until Ctrl-C, then unmount cleanly.
    let drive = ds_mount_winfsp::mount(fs.clone(), &mountpoint, export)?;
    // Server events → cache invalidation (in the pump) → the notify timer
    // re-emits them as real ReadDirectoryChangesW notifications.
    let sink = drive.event_sink();
    fs.start_event_pump(move |batch| sink.push(batch)).await?;
    tracing::info!(mountpoint, "mounted; press Ctrl-C to unmount");
    tokio::signal::ctrl_c().await?;
    tracing::info!("unmounting");
    drive.unmount();
    fs.shutdown(); // persist the auto-cache manifest
    Ok(())
}
