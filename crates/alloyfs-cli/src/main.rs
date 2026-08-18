//! alloyfs CLI: clap surface + dispatch. All behavior lives in
//! `commands/`; parsing/config/urls in their own modules.

mod commands;
mod config;
mod urls;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "alloyfs", version, about = "Cross-platform virtual drive service")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the agent, serving exports to clients.
    Serve {
        /// TCP listen address. Defaults to agent.tcp_listen from the config,
        /// then 127.0.0.1:7440. No clap default, so that passing this flag can
        /// actually override the config — with one, every run would look like
        /// an explicit choice and the config could never win.
        #[arg(long)]
        tcp: Option<String>,
        /// Serve one session over stdin/stdout instead of TCP (what
        /// `mount ssh://...` runs on the remote side).
        #[arg(long)]
        stdio: bool,
        /// Config file (.yml/.yaml preferred, .toml supported).
        #[arg(long)]
        config: Option<PathBuf>,
        /// Inline export, NAME=PATH (repeatable). Alternative to --config.
        #[arg(long = "export", value_name = "NAME=PATH")]
        exports: Vec<String>,
    },
    /// Mount an export as a local drive.
    ///
    /// Two positionals mount a url somewhere:
    ///     alloyfs mount tcp://host:7440/projects P:
    /// One mounts a name from `client.mounts` in the config, which brings its
    /// own url, mountpoint and settings:
    ///     alloyfs mount work
    ///
    /// URL: tcp://host:port/export or ssh://host/export
    // `verbatim_doc_comment` so the two example lines survive as lines instead
    // of being rewrapped into one paragraph; `about` spelled out because that
    // setting would otherwise carry the trailing full stop into the subcommand
    // list, where no other entry has one.
    #[command(verbatim_doc_comment, about = "Mount an export as a local drive")]
    Mount {
        /// A mount url when a MOUNTPOINT follows it; the name of a mount under
        /// `client.mounts` in the config when nothing does.
        #[arg(value_name = "URL|NAME")]
        target: String,
        /// Mountpoint: a directory on Linux, a drive letter (X:) on Windows.
        /// Left out, the first positional is read as a configured mount name.
        mountpoint: Option<PathBuf>,
        /// Remote alloyfs command for ssh:// urls.
        #[arg(long, default_value = "alloyfs")]
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
        /// Ignore the server's suggested client settings (exclude/pin/cache
        /// sizes published by the export's `client:` config section).
        #[arg(long)]
        no_server_defaults: bool,
        /// Refuse to write over a file another machine changed since this
        /// mount last read it, instead of silently winning. Off by default:
        /// it trades a failed save for a lost edit, and which of those you
        /// prefer depends on what the mount is for.
        #[arg(long)]
        detect_conflicts: bool,
        /// Shared secret for token-protected TCP servers (agent.tcp_token).
        #[arg(long)]
        token: Option<String>,
        /// Linux mount driver. `fuse` works everywhere; `kernel` uses the
        /// alloyfs module (real inotify events for remote changes) and needs
        /// /dev/alloyfs plus CAP_SYS_ADMIN.
        #[arg(long, value_enum, default_value_t = commands::mount::Backend::Fuse)]
        backend: commands::mount::Backend,
    },
    /// Keep a REAL local directory bidirectionally in sync with an export.
    /// Unlike a mount, watchers on the directory get genuine file events for
    /// remote changes (this is the Linux answer to FUSE's inotify limit).
    /// url: tcp://host:port/export or ssh://host/export
    Sync {
        url: String,
        /// Local directory (created if missing).
        dir: PathBuf,
        /// Remote alloyfs command for ssh:// urls.
        #[arg(long, default_value = "alloyfs")]
        remote_cmd: String,
        /// Paths to skip, both directions (gitignore-style glob, repeatable).
        #[arg(long = "exclude", value_name = "GLOB")]
        excludes: Vec<String>,
        /// Local change debounce in milliseconds.
        #[arg(long, default_value_t = 250)]
        debounce_ms: u64,
        /// Conflict policy: newer (last writer wins), remote, or local.
        /// The losing copy is always kept as <name>.sync-conflict-<ts>.
        #[arg(long, default_value = "newer")]
        conflict_policy: String,
        /// Where the sync manifest lives. Default: per-user app data.
        #[arg(long, value_name = "PATH")]
        data_dir: Option<PathBuf>,
        /// Shared secret for token-protected TCP servers.
        #[arg(long)]
        token: Option<String>,
        /// Reconcile once and exit (no live watching).
        #[arg(long)]
        one_shot: bool,
    },
    /// Start everything this machine's config describes: the agent from
    /// `server:`, plus every mount under `client.mounts:`. Runs until Ctrl-C.
    Start {
        /// Config file. Defaults to the discovered one.
        #[arg(long)]
        config: Option<PathBuf>,
        /// Only the agent, no mounts.
        #[arg(long, conflicts_with = "mounts_only")]
        server_only: bool,
        /// Only the mounts, no agent.
        #[arg(long)]
        mounts_only: bool,
    },
    /// Mounts and agents that start on their own, with no terminal window.
    /// Every subcommand except `list` needs an ELEVATED shell.
    Service {
        #[command(subcommand)]
        cmd: ServiceCmd,
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
        /// Remote alloyfs command for ssh:// urls.
        #[arg(long, default_value = "alloyfs")]
        remote_cmd: String,
        /// Shared secret for token-protected TCP servers.
        #[arg(long)]
        token: Option<String>,
    },
    /// Measure round-trip latency to an agent (url: tcp://host:port or ssh://host).
    Ping {
        url: String,
        #[arg(long, default_value_t = 5)]
        count: u32,
        /// Shared secret for token-protected TCP servers.
        #[arg(long)]
        token: Option<String>,
    },
    /// Fire many concurrent pipelined requests at an agent and verify replies.
    Stress {
        url: String,
        #[arg(long, default_value_t = 1000)]
        count: u32,
        /// Shared secret for token-protected TCP servers.
        #[arg(long)]
        token: Option<String>,
    },
    /// Timed pipelined read of one remote file (no kernel mount involved).
    /// url: tcp://host:port/export or ssh://host/export.
    Bench {
        url: String,
        /// File path inside the export.
        path: String,
        /// Concurrent chunk requests (1 = serial; 16 = one readahead window).
        #[arg(long, default_value_t = 16)]
        depth: usize,
        #[arg(long, default_value = "alloyfs")]
        remote_cmd: String,
        /// Shared secret for token-protected TCP servers.
        #[arg(long)]
        token: Option<String>,
    },
    /// Write a config for a directory, ready to serve.
    Init {
        /// Directory to export (default: the current one).
        dir: Option<PathBuf>,
        /// Export name (default: derived from the directory name).
        #[arg(long)]
        name: Option<String>,
        /// Write ~/.alloyfs/config.yml instead of ./alloyfs.yml.
        #[arg(long)]
        global: bool,
        /// Overwrite an existing file.
        #[arg(long)]
        force: bool,
    },
    /// Update AlloyFS in place by re-running the official installer.
    Update {
        /// `stable` (the default), or a tag like `v0.1.1` to pin or roll back.
        channel: Option<String>,
        /// Print the command that would run, and stop.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
enum ServiceCmd {
    /// One-time preparation: check WinFsp, create and lock down the instance
    /// directory. Safe to re-run.
    Setup,
    /// Define an instance and register it to start at boot.
    ///
    /// Boxed because clap gives every field of every variant to the enum, and
    /// this one dwarfs `Remove { id }` — the whole `ServiceCmd` would be sized
    /// by its largest arm otherwise.
    Add(Box<AddArgs>),
    /// Stop, unregister, and forget an instance.
    Remove { id: String },
    /// Start one instance, or every instance.
    Start { id: Option<String> },
    /// Stop one instance, or every instance.
    Stop { id: Option<String> },
    /// Restart one instance, or every instance.
    Restart { id: Option<String> },
    /// What is defined, and what each one is doing.
    List,
    /// Remove EVERY instance and its definition.
    Reset {
        #[arg(long)]
        confirm: bool,
    },
    /// Internal: the entry point the Windows service manager invokes.
    #[command(hide = true)]
    Run { id: String },
}

/// The arguments of `service add`, in their own struct so the variant that
/// carries them can be boxed.
#[derive(clap::Args)]
struct AddArgs {
    /// Name for this instance (letters, digits, dashes, underscores).
    id: String,
    /// Mount url: tcp://host:port/export or ssh://host/export.
    #[arg(long, conflicts_with = "config")]
    url: Option<String>,
    /// Where to mount it: a drive letter (P:) on Windows.
    #[arg(long, requires = "url")]
    mount: Option<PathBuf>,
    /// Run an AGENT with this config instead of mounting.
    #[arg(long, conflicts_with_all = ["url", "mount"])]
    config: Option<PathBuf>,
    /// Agent TCP listen address (with --config, or alone).
    #[arg(long, conflicts_with = "url")]
    tcp: Option<String>,
    /// Local-only paths, never sent to the server (repeatable).
    #[arg(long = "exclude", value_name = "GLOB", requires = "url")]
    excludes: Vec<String>,
    /// Always fully cache matching files (repeatable).
    #[arg(long = "pin", value_name = "GLOB", requires = "url")]
    pins: Vec<String>,
    /// Auto-download files up to this size ("2M", 0 = off).
    #[arg(long, requires = "url")]
    auto_cache_max: Option<String>,
    /// Total local cache budget.
    #[arg(long, requires = "url")]
    auto_cache_budget: Option<String>,
    /// Refuse to overwrite a file another machine changed.
    #[arg(long, requires = "url")]
    detect_conflicts: bool,
    /// Start it now as well as at boot.
    #[arg(long)]
    start: bool,
}
#[derive(Subcommand)]
enum CacheCmd {
    /// Delete cached blobs for one mount url (SAFE while unmounted; never
    /// touches the overlay).
    Clear {
        /// Mount url incl. export (e.g. ssh://azure/projects), or --all.
        target: Option<String>,
        #[arg(long)]
        all: bool,
        #[arg(long, value_name = "PATH")]
        data_dir: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr) // stdout stays clean: --stdio transport uses it
        .init();

    match Cli::parse().command {
        Command::Serve {
            tcp,
            stdio,
            config,
            exports,
        } => commands::serve::run(tcp, stdio, config, exports).await,
        Command::Mount {
            target,
            mountpoint,
            remote_cmd,
            config,
            excludes,
            pins,
            auto_cache_max,
            auto_cache_budget,
            data_dir,
            no_server_defaults,
            detect_conflicts,
            token,
            backend,
        } => {
            commands::mount::run_cli(
                target,
                mountpoint,
                remote_cmd,
                config,
                excludes,
                pins,
                auto_cache_max,
                auto_cache_budget,
                data_dir,
                no_server_defaults,
                detect_conflicts,
                token,
                backend,
            )
            .await
        }
        Command::Sync {
            url,
            dir,
            remote_cmd,
            excludes,
            debounce_ms,
            conflict_policy,
            data_dir,
            token,
            one_shot,
        } => {
            commands::sync::run(
                url,
                dir,
                remote_cmd,
                excludes,
                debounce_ms,
                conflict_policy,
                data_dir,
                token,
                one_shot,
            )
            .await
        }
        Command::Start {
            config,
            server_only,
            mounts_only,
        } => commands::start::run(config, server_only, mounts_only).await,
        Command::Service { cmd } => match cmd {
            ServiceCmd::Setup => commands::service::setup(),
            ServiceCmd::Add(args) => {
                use commands::service::Instance;
                let AddArgs {
                    id,
                    url,
                    mount,
                    config,
                    tcp,
                    excludes,
                    pins,
                    auto_cache_max,
                    auto_cache_budget,
                    detect_conflicts,
                    start,
                } = *args;
                // clap enforces that --mount accompanies --url and that the
                // agent flags do not mix with them; what it cannot express is
                // "one of the two shapes must be chosen".
                let instance = match (url, config, tcp) {
                    (Some(url), _, _) => Instance::Mount {
                        url,
                        mountpoint: mount.ok_or_else(|| anyhow::anyhow!("--url needs --mount"))?,
                        exclude: excludes,
                        pin: pins,
                        auto_cache_max,
                        auto_cache_budget,
                        detect_conflicts,
                    },
                    (None, config @ Some(_), tcp) => Instance::Agent { config, tcp },
                    (None, None, tcp @ Some(_)) => Instance::Agent { config: None, tcp },
                    (None, None, None) => anyhow::bail!(
                        "say what to run: --url URL --mount P: for a drive, or --config PATH for an agent"
                    ),
                };
                commands::service::add(id, instance, start)
            }
            ServiceCmd::Remove { id } => commands::service::remove(id),
            ServiceCmd::Start { id } => commands::service::control("start", id),
            ServiceCmd::Stop { id } => commands::service::control("stop", id),
            ServiceCmd::Restart { id } => commands::service::control("restart", id),
            ServiceCmd::List => commands::service::list(),
            ServiceCmd::Reset { confirm } => commands::service::reset(confirm),
            #[cfg(windows)]
            ServiceCmd::Run { id } => commands::service::runtime::run(&id),
            #[cfg(not(windows))]
            ServiceCmd::Run { id } => {
                let _ = id;
                anyhow::bail!("`service run` is Windows-only")
            }
        },
        Command::Cache { cmd } => match cmd {
            CacheCmd::Clear {
                target,
                all,
                data_dir,
            } => commands::cache::clear(target, all, data_dir),
        },
        Command::Events {
            url,
            since,
            remote_cmd,
            token,
        } => commands::diag::events(url, since, remote_cmd, token).await,
        Command::Ping { url, count, token } => commands::diag::ping(url, count, token).await,
        Command::Stress { url, count, token } => commands::diag::stress(url, count, token).await,
        Command::Bench {
            url,
            path,
            depth,
            remote_cmd,
            token,
        } => commands::diag::bench(url, path, depth, remote_cmd, token).await,
        Command::Init {
            dir,
            name,
            global,
            force,
        } => commands::init::run(dir, name, global, force),
        Command::Update { channel, dry_run } => commands::update::run(channel, dry_run),
    }
}
