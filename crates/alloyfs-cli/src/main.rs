//! alloyfs CLI: clap surface + dispatch. All behavior lives in
//! `commands/`; parsing/config/urls in their own modules.

mod commands;
mod config;
mod logfile;
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
        /// Every mutation blocks until the server has it (the pre-v10
        /// contract). Without this flag, bursts of NEW small files and
        /// removals acknowledge locally and coalesce for at most ~15 ms into
        /// bulk exchanges; fsync, locks, renames and unmount still block for
        /// the server either way.
        #[arg(long)]
        write_through: bool,
        /// Compress this mount's outgoing large frames with zstd instead of
        /// the default lz4 (needs a v13+ server; silently stays lz4 below).
        /// The server's own direction opts in via its config (`zstd: true`).
        #[arg(long)]
        zstd: bool,
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
    /// Fetch an export.s whole tree in one exchange and report what it cost
    /// (url: tcp://host:port/export or ssh://host/export).
    Tree {
        url: String,
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
    /// Small-file fetch, per-file against ReadMany, alternating in one process.
    /// url: tcp://host:port/export or ssh://host/export.
    Bulk {
        url: String,
        /// Directory inside the export whose files are fetched.
        #[arg(default_value = "")]
        dir: String,
        /// Rounds of each strategy; the median is reported.
        #[arg(long, default_value_t = 3)]
        rounds: usize,
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
        /// Report whether a newer release exists and stop. Exits 1 when one
        /// does, so a script can act on the status alone.
        #[arg(long, conflicts_with_all = ["dry_run", "rollback"])]
        check: bool,
        /// Put back the binary kept by the last update. No network needed —
        /// which is the point, when the new release is what broke.
        #[arg(long, conflicts_with = "dry_run")]
        rollback: bool,
    },
    /// Check the local things that stop a drive from working.
    Doctor,
    /// Inspect the configuration file.
    Config {
        #[command(subcommand)]
        cmd: ConfigCmd,
    },
    /// Read what a long-running command logged. No name lists what is there.
    Logs {
        /// Log name: a service id, or `serve` / `mount` / `start`.
        name: Option<String>,
        /// Keep printing as the file grows.
        #[arg(short, long)]
        follow: bool,
        /// Trailing lines to print first.
        #[arg(short = 'n', long, default_value_t = 200)]
        lines: usize,
    },
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Parse the config and report what it describes.
    Validate {
        #[arg(long, value_name = "PATH")]
        config: Option<PathBuf>,
    },
    /// Print the settled values, after client defaults and per-mount
    /// overrides have been merged.
    Print {
        #[arg(long, value_name = "PATH")]
        config: Option<PathBuf>,
        /// Only this mount.
        #[arg(long)]
        mount: Option<String>,
    },
}

#[derive(Subcommand)]
enum ServiceCmd {
    /// One-time preparation: check the filesystem driver, create the instance
    /// directory and restrict it. Safe to re-run.
    Setup,
    /// Register a service that runs part of this machine's config at boot.
    ///
    /// With no flags it runs `alloyfs start`: the agent, then every mount. The
    /// mounts themselves stay in the config — a service records WHICH part to
    /// run, never a copy of the settings.
    // Boxed because clap gives every field of every variant to the enum, and
    // this one dwarfs `Remove { id }` — the whole `ServiceCmd` would be sized
    // by its largest arm otherwise. Not a doc comment: clap prints those, and
    // a layout note is not something `service add --help` should explain.
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
    /// Internal: the entry point the Windows service control manager invokes.
    ///
    /// Windows-only by nature rather than by omission. It exists to supervise a
    /// child in another session; systemd starts `alloyfs` itself and supervises
    /// it directly, so on Linux there is nothing for this to be.
    #[command(hide = true)]
    Run { id: String },
}

/// The arguments of `service add`, in their own struct so the variant that
/// carries them can be boxed.
#[derive(clap::Args)]
struct AddArgs {
    /// Name for this instance (letters, digits, dashes, underscores).
    id: String,
    /// Run ONE mount from `client.mounts` instead of the whole config,
    /// named the way `alloyfs mount <NAME>` names it.
    #[arg(long, value_name = "NAME")]
    mount: Option<String>,
    /// Only the agent from `server:`, no mounts.
    #[arg(long, conflicts_with_all = ["mount", "mounts_only"])]
    server_only: bool,
    /// Only the mounts under `client.mounts:`, no agent — for a machine where
    /// something else already serves.
    #[arg(long, conflicts_with = "mount")]
    mounts_only: bool,
    /// Config to run from. Default: whichever one the logged-in user
    /// discovers at launch. The file is opened by THEM, not by the service,
    /// so it has to be a path they can read.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
    /// Start it now as well as at boot.
    #[arg(long)]
    start: bool,
    #[command(flatten)]
    copied: CopiedArgs,
}

/// The flags a service definition used to copy into a file of its own.
///
/// Hidden, and accepted only to answer an old command line with a sentence
/// instead of `unexpected argument '--url' found`. Every one of them now
/// belongs in the config, and the published examples used them.
#[derive(clap::Args)]
struct CopiedArgs {
    #[arg(long, hide = true)]
    url: Option<String>,
    #[arg(long = "exclude", hide = true)]
    excludes: Vec<String>,
    #[arg(long = "pin", hide = true)]
    pins: Vec<String>,
    #[arg(long, hide = true)]
    auto_cache_max: Option<String>,
    #[arg(long, hide = true)]
    auto_cache_budget: Option<String>,
    #[arg(long, hide = true)]
    detect_conflicts: bool,
    #[arg(long, hide = true)]
    tcp: Option<String>,
}

impl CopiedArgs {
    /// Which of these were given, and where each one lives now.
    fn moved(&self) -> Vec<(&'static str, &'static str)> {
        let mut out = Vec::new();
        if self.url.is_some() {
            out.push(("--url", "client.mounts.<name>.url"));
        }
        if !self.excludes.is_empty() {
            out.push(("--exclude", "client.mounts.<name>.exclude, or client.exclude"));
        }
        if !self.pins.is_empty() {
            out.push(("--pin", "client.mounts.<name>.pin, or client.pin"));
        }
        if self.auto_cache_max.is_some() {
            out.push(("--auto-cache-max", "client.auto_cache_max"));
        }
        if self.auto_cache_budget.is_some() {
            out.push(("--auto-cache-budget", "client.auto_cache_budget"));
        }
        if self.detect_conflicts {
            out.push(("--detect-conflicts", "client.detect_conflicts"));
        }
        if self.tcp.is_some() {
            out.push(("--tcp", "server.tcp_listen"));
        }
        out
    }

    /// The error an old command line earns, naming every flag it used.
    fn migration_error(&self) -> Option<anyhow::Error> {
        let moved = self.moved();
        if moved.is_empty() {
            return None;
        }
        let mut lines = String::new();
        for (flag, home) in &moved {
            lines.push_str(&format!("\n\x20   {flag:<20} -> {home}"));
        }
        Some(anyhow::anyhow!(
            "a service no longer keeps its own copy of a mount. These moved into the config:\
             {lines}\n\
             \n\
             \x20 client:\n\
             \x20   mounts:\n\
             \x20     work:\n\
             \x20       url: ssh://azure/projects\n\
             \x20       at: \"P:\"\n\
             \n\
             \x20 Then register a service that points at it:\n\
             \x20   alloyfs service add work --mount work\n\
             \x20 or one that runs everything the config describes:\n\
             \x20   alloyfs service add alloyfs"
        ))
    }
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

fn main() -> anyhow::Result<()> {
    // Not #[tokio::main]: the defaults are sized for servers with memory to
    // burn — up to 512 blocking threads at 2 MiB of stack apiece. Every
    // filesystem request the agent serves funnels through spawn_blocking,
    // so one burst (a client's multi-stream readahead plus a WriteMany
    // flush) could materialize hundreds of OS threads on the sub-gigabyte
    // boxes agents actually run on. 32 blocking threads is already past
    // the useful disk parallelism of those machines, and 1 MiB of stack
    // clears every path here — shallow fs syscalls, tracing, postcard's
    // iterative codec — with room to spare.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .max_blocking_threads(32)
        .thread_stack_size(1024 * 1024)
        .build()?;
    rt.block_on(async_main())
}

/// The log file this run writes to, or `None` for commands that finish
/// while their terminal is still open.
///
/// Only the long-running ones earn a file: those are the commands whose
/// story outlives the session that started them, and the ones a service
/// runs with no console at all.
fn log_name(command: &Command) -> Option<String> {
    if let Some(name) = logfile::name_override() {
        return Some(name);
    }
    match command {
        Command::Serve { .. } => Some("serve".into()),
        Command::Mount { .. } => Some("mount".into()),
        Command::Start { .. } => Some("start".into()),
        Command::Sync { .. } => Some("sync".into()),
        // The Windows supervisor's own diagnostics — the backoff messages
        // that used to go nowhere at all.
        Command::Service {
            cmd: ServiceCmd::Run { id },
        } => Some(id.clone()),
        _ => None,
    }
}

fn init_tracing(log_name: Option<String>) {
    use tracing_subscriber::layer::SubscriberExt as _;
    use tracing_subscriber::util::SubscriberInitExt as _;

    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    // stdout stays clean: the --stdio transport uses it.
    let terminal = tracing_subscriber::fmt::layer().with_writer(std::io::stderr);
    // No ANSI in the file: `alloyfs logs` prints it verbatim, and escape
    // codes in a log someone pastes into an issue help nobody.
    let file = log_name.map(|name| {
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(logfile::open(&name))
    });
    tracing_subscriber::registry()
        .with(filter)
        .with(terminal)
        .with(file)
        .init();
}

async fn async_main() -> anyhow::Result<()> {
    // Parsed before logging is set up: the command decides whether this run
    // keeps a log file, and clap's own errors need no subscriber.
    let command = Cli::parse().command;
    init_tracing(log_name(&command));

    match command {
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
            write_through,
            zstd,
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
                write_through,
                zstd,
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
                use commands::service::{Instance, Scope};
                let AddArgs {
                    id,
                    mount,
                    server_only,
                    mounts_only,
                    config,
                    start,
                    copied,
                } = *args;
                if let Some(e) = copied.migration_error() {
                    return Err(e);
                }
                // No flags at all is the whole config, which is the shape
                // worth defaulting to: one service, one definition, and
                // nothing to keep in step.
                let instance = match mount {
                    Some(name) => Instance::Mount { name, config },
                    None => Instance::Start {
                        config,
                        scope: match (server_only, mounts_only) {
                            (true, _) => Scope::Server,
                            (_, true) => Scope::Mounts,
                            _ => Scope::All,
                        },
                    },
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
            ServiceCmd::Run { id } => anyhow::bail!(
                "`service run` is the Windows service control manager's entry point, and has \
                 no counterpart here: systemd runs `alloyfs {}` itself.\n\
                 \n\
                 \x20 systemctl --user start alloyfs-{id}      # or: alloyfs service start {id}\n\
                 \x20 journalctl --user -u alloyfs-{id} -f",
                commands::service::instance::load(&id)
                    .map(|i| i.command())
                    .unwrap_or_else(|_| "start".into())
            ),
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
        Command::Tree {
            url,
            remote_cmd,
            token,
        } => commands::diag::tree(url, remote_cmd, token).await,
        Command::Ping { url, count, token } => commands::diag::ping(url, count, token).await,
        Command::Stress { url, count, token } => commands::diag::stress(url, count, token).await,
        Command::Bulk {
            url,
            dir,
            rounds,
            remote_cmd,
            token,
        } => commands::diag::bulk(url, dir, rounds, remote_cmd, token).await,
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
        Command::Update {
            channel,
            dry_run,
            check,
            rollback,
        } => match (check, rollback) {
            (true, _) => commands::update::check(),
            (_, true) => commands::update::rollback(),
            _ => commands::update::run(channel, dry_run),
        },
        Command::Doctor => commands::doctor::run(),
        Command::Config { cmd } => match cmd {
            ConfigCmd::Validate { config } => commands::config_cmd::validate(config),
            ConfigCmd::Print { config, mount } => commands::config_cmd::print(config, mount),
        },
        Command::Logs { name, follow, lines } => commands::logs::run(name, follow, lines).await,
    }
}
