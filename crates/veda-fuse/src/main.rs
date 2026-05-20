use veda_fuse::{client, commit_queue, fs, shadow, sse};

use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use clap::{Parser, Subcommand, ValueEnum};
use tracing::info;

#[derive(Clone, Copy, ValueEnum, Debug, PartialEq, Eq)]
enum WriteMode {
    /// Every flush/fsync/release blocks on a server PUT. Day-0
    /// behaviour; keep as default until writeback bakes in.
    Sync,
    /// Writes land in a local in-memory shadow buffer. The commit
    /// queue debounces and pushes them server-side after
    /// `--write-debounce-ms` of quiet. vim swap / git lockfile / IDE
    /// temp files don't pollute the server.
    Writeback,
}

#[derive(Parser)]
#[command(name = "veda-fuse", about = "Mount a Veda workspace as a local filesystem")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Mount a Veda workspace
    Mount(MountArgs),
    /// Unmount a Veda workspace
    Umount(UmountArgs),
}

#[derive(Parser)]
struct MountArgs {
    #[arg(long, env = "VEDA_SERVER")]
    server: String,

    #[arg(long, env = "VEDA_KEY")]
    key: String,

    /// Mount point path
    mountpoint: String,

    /// Run in foreground (don't daemonize)
    #[arg(long, default_value = "false")]
    foreground: bool,

    /// Read cache size in MB
    #[arg(long, default_value = "128")]
    cache_size: usize,

    /// FileAttr cache TTL in seconds. SSE invalidations refresh
    /// individual entries earlier; this is the safety-net upper
    /// bound. 30s is comfortable for git-status / make-style stat
    /// storms.
    #[arg(long, default_value = "30")]
    attr_ttl: u64,

    /// Directory listing cache TTL in seconds. Longer than
    /// `--attr-ttl` because dir entries churn less than file
    /// mtimes; SSE invalidates on add/remove/rename.
    #[arg(long, default_value = "60")]
    dir_ttl: u64,

    /// Allow other users to access the mount
    #[arg(long, default_value = "false")]
    allow_other: bool,

    /// Mount as read-only
    #[arg(long, default_value = "false")]
    read_only: bool,

    /// Enable FUSE debug logging
    #[arg(long, default_value = "false")]
    debug: bool,

    /// Write mode: `sync` (default) or `writeback` (debounced commits
    /// via local shadow buffer). `VEDA_FUSE_WRITE_MODE` overrides.
    #[arg(long, env = "VEDA_FUSE_WRITE_MODE", value_enum, default_value_t = WriteMode::Sync)]
    write_mode: WriteMode,

    /// Debounce window for writeback commits (only relevant in
    /// writeback mode). `VEDA_FUSE_WRITE_DEBOUNCE_MS` overrides.
    #[arg(long, env = "VEDA_FUSE_WRITE_DEBOUNCE_MS", default_value = "5000")]
    write_debounce_ms: u64,
}

#[derive(Parser)]
struct UmountArgs {
    /// Mount point path
    mountpoint: String,
}

use std::time::Duration;

struct MountOpts {
    foreground: bool,
    allow_other: bool,
    read_only: bool,
    write_mode: WriteMode,
    write_debounce_ms: u64,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Cmd::Mount(args) => cmd_mount(args),
        Cmd::Umount(args) => cmd_umount(args),
    }
}

fn cmd_mount(args: MountArgs) -> anyhow::Result<()> {
    // No pre-flight stat here — observed behaviour on macOS
    // (Darwin 24.6): building a reqwest::blocking::Client in the
    // parent and running it before fork() makes the eventual fork
    // child crash with SIGILL before tracing initialises. Removing
    // the pre-fork client makes daemon mount succeed. The exact
    // mechanism is unconfirmed (likely some post-fork state from
    // the tokio runtime that reqwest spawns under the hood), but
    // the operational rule is clear: do not touch reqwest in the
    // parent. Foreground keeps its inline preflight (no fork at
    // all); daemon mode runs preflight in the child and pipes the
    // error back to the parent.

    let config = fs::FuseConfig {
        attr_ttl: Duration::from_secs(args.attr_ttl),
        dir_ttl: Duration::from_secs(args.dir_ttl),
        read_only: args.read_only,
        cache_size_mb: args.cache_size,
    };

    let mount_opts = MountOpts {
        foreground: args.foreground,
        allow_other: args.allow_other,
        read_only: args.read_only,
        write_mode: args.write_mode,
        write_debounce_ms: args.write_debounce_ms,
    };

    if args.foreground {
        init_tracing(args.debug);
        info!(mount = %args.mountpoint, server = %args.server, "mounting (foreground)");
        // Foreground is single-process — safe to build the client here.
        let client = Arc::new(client::VedaClient::new(&args.server, &args.key));
        // Preflight inline: typos in --server fail at exit-1 before
        // fuser::mount2 ties up the mountpoint.
        client.stat("/").map_err(|e| {
            anyhow::anyhow!("server health check failed (stat '/' at {}): {e}", args.server)
        })?;
        return mount_and_serve(client, &args.mountpoint, &args.server, &args.key, config, &mount_opts, None);
    }

    // Daemonize: parent waits for child to signal readiness via pipe.
    // CRITICAL: the HTTP client must be constructed AFTER fork in the
    // child. Building it before fork left the daemon with a tokio
    // worker thread that lived only in the parent and a connection
    // pool of fds that became stale in the child — every FUSE op then
    // returned I/O error because no HTTP request could complete.
    let (read_fd, write_fd) = nix::unistd::pipe()?;
    let write_raw = write_fd.as_raw_fd();

    // Resolve the daemon log path in the parent so it can be reported
    // up front (parent prints location; child writes its stderr there).
    let log_path = daemon_log_path();

    match unsafe { nix::unistd::fork() }? {
        nix::unistd::ForkResult::Parent { child } => {
            drop(write_fd);
            if let Some(ref p) = log_path {
                eprintln!("veda: daemon log → {}", p.display());
            }
            // Hand the OwnedFd to wait_for_child_ready so it owns the
            // close. Passing only the raw fd while keeping `read_fd`
            // in scope would double-close on drop.
            wait_for_child_ready(read_fd, child, log_path.as_deref())
        }
        nix::unistd::ForkResult::Child => {
            drop(read_fd);
            nix::unistd::setsid()?;

            // Detach stdin/stdout from the parent terminal so the
            // calling shell (and any ssh session above it) can return
            // cleanly — without this, ssh hangs waiting for the
            // inherited fds to close. stderr goes to a daemon log
            // file (append) instead of /dev/null so tracing output
            // survives mount failures; otherwise the daemon would
            // exit silently and the parent's "exited before ready"
            // error gives no clue what went wrong.
            redirect_stdio_for_daemon(log_path.as_deref());

            init_tracing(args.debug);
            info!(mount = %args.mountpoint, server = %args.server, "mounting (background)");

            let client = Arc::new(client::VedaClient::new(&args.server, &args.key));
            // Preflight stat AFTER fork. The parent does NOT run any
            // reqwest call before fork on macOS — see the long
            // comment at the top of cmd_mount. Running it here is
            // safe (single-threaded post-fork until SSE/signal
            // threads spawn inside mount_and_serve), and the
            // readiness pipe carries the error back to the parent
            // so the user still gets fail-fast UX.
            if let Err(e) = client.stat("/") {
                let msg = format!("server health check failed (stat '/' at {}): {e}", args.server);
                tracing::error!("{msg}");
                // Write the message to the readiness pipe so the
                // parent prints it instead of "exit 1, check log".
                // First byte must be != 'R' so the parent knows this
                // is an error frame, not a successful mount.
                let _ = nix::unistd::write(
                    unsafe { std::os::fd::BorrowedFd::borrow_raw(write_raw) },
                    msg.as_bytes(),
                );
                drop(write_fd);
                std::process::exit(1);
            }

            let result = mount_and_serve(client, &args.mountpoint, &args.server, &args.key, config, &mount_opts, Some(write_raw));
            if let Err(ref e) = result {
                tracing::error!("mount failed: {e}");
            }
            std::mem::forget(write_fd);
            std::process::exit(if result.is_ok() { 0 } else { 1 });
        }
    }
}

fn build_fuse_options(opts: &MountOpts) -> Vec<fuser::MountOption> {
    let mut options = vec![
        fuser::MountOption::FSName("veda".to_string()),
        fuser::MountOption::DefaultPermissions,
    ];
    if cfg!(target_os = "macos") {
        options.push(fuser::MountOption::CUSTOM("noappledouble".into()));
        options.push(fuser::MountOption::CUSTOM("noapplexattr".into()));
    }
    if opts.foreground {
        options.push(fuser::MountOption::AutoUnmount);
    }
    if opts.allow_other {
        options.push(fuser::MountOption::AllowOther);
    }
    if opts.read_only {
        options.push(fuser::MountOption::RO);
    }
    options
}

fn mount_and_serve(
    client: Arc<client::VedaClient>,
    mountpoint: &str,
    server: &str,
    key: &str,
    config: fs::FuseConfig,
    opts: &MountOpts,
    notify_fd: Option<RawFd>,
) -> anyhow::Result<()> {
    // Probe the server's summary capability before mounting so
    // readdir can decide whether to advertise `.abstract` /
    // `.overview` sidecars. Fail-open: any error (network glitch,
    // older server without the endpoint) defaults to advertising —
    // the per-directory miss cache mops up phantoms reactively. See
    // crates/veda-fuse/src/fs.rs `sidecar_recently_missing`.
    let summary_enabled = match client.get_capabilities() {
        Ok(caps) => caps.summary_enabled,
        Err(e) => {
            tracing::warn!(err = %e, "capability probe failed; defaulting summary_enabled=true");
            true
        }
    };
    // Build the shadow store + commit queue only in writeback mode.
    // sync mode passes None for both → fs.rs falls through to the
    // pre-existing legacy path on every write/flush/release.
    let (shadow_opt, commit_queue_opt) = if opts.write_mode == WriteMode::Writeback {
        let shadow = Arc::new(Mutex::new(shadow::ShadowStore::new()));
        let cq_client: Arc<dyn commit_queue::CommitClient> = client.clone();
        let queue = commit_queue::CommitQueue::start(
            cq_client,
            shadow.clone(),
            std::time::Duration::from_millis(opts.write_debounce_ms),
        );
        info!(window_ms = opts.write_debounce_ms, "writeback mode enabled");
        (Some(shadow), Some(queue))
    } else {
        (None, None)
    };
    let vedafs = fs::VedaFs::new(
        client,
        config,
        summary_enabled,
        notify_fd,
        shadow_opt,
        commit_queue_opt,
    );
    let cursor_file = sse::cursor_file_path(server);
    let mut watcher = sse::SseWatcher::start(
        server, key,
        vedafs.inodes(), vedafs.read_cache(), vedafs.dir_cache(),
        vedafs.sidecar_miss(),
        cursor_file,
    );
    let options = build_fuse_options(opts);
    install_signal_handler(mountpoint.to_string());
    let result = fuser::mount2(vedafs, mountpoint, &options);
    watcher.stop();
    result.map_err(Into::into)
}

/// Standard log file path for the daemon's stderr stream:
/// `${XDG_CACHE_HOME:-$HOME/.cache}/veda-fuse/daemon.log`, same XDG
/// convention as veda-cli's config. Returns None if HOME is unset or
/// the directory cannot be created — in that case the daemon falls
/// back to /dev/null.
fn daemon_log_path() -> Option<PathBuf> {
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
    let dir = cache.join("veda-fuse");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join("daemon.log"))
}

/// Detach the daemon child from the parent terminal. stdin goes to
/// /dev/null. stdout AND stderr both go to `log_path` in append mode
/// if provided — tracing-subscriber's default writer is stdout, so
/// covering both means tracing output lands in the log regardless of
/// subscriber config. Falling back to /dev/null keeps the daemon
/// starting even if the log file can't be opened.
fn redirect_stdio_for_daemon(log_path: Option<&Path>) {
    use std::os::fd::AsRawFd;

    if let Ok(f) = std::fs::OpenOptions::new()
        .read(true)
        .open("/dev/null")
    {
        unsafe { libc::dup2(f.as_raw_fd(), 0); }
    }

    let out_fd = log_path
        .and_then(|p| std::fs::OpenOptions::new().create(true).append(true).open(p).ok())
        .or_else(|| std::fs::OpenOptions::new().write(true).open("/dev/null").ok());
    if let Some(f) = out_fd {
        unsafe {
            libc::dup2(f.as_raw_fd(), 1);
            libc::dup2(f.as_raw_fd(), 2);
        }
        // f drops here; fds 1/2 point at independent dup entries.
    }
}

fn wait_for_child_ready(
    read_fd: OwnedFd,
    child: nix::unistd::Pid,
    log_path: Option<&Path>,
) -> anyhow::Result<()> {
    use std::io::Read;
    // OwnedFd → File: File now owns the fd, drops at end of fn. The
    // original `read_fd` is moved here so no double-close.
    let mut pipe_read = std::fs::File::from(read_fd);
    let mut first = [0u8; 1];

    match pipe_read.read(&mut first) {
        Ok(1) if first[0] == b'R' => {
            eprintln!("veda: mounted (pid {})", child);
            Ok(())
        }
        Ok(1) => {
            // Any non-'R' first byte signals a structured error
            // frame: the child sends the human-readable message
            // through the pipe and then closes it. Read the rest,
            // reap the child so it doesn't linger as a zombie, and
            // propagate the message.
            let mut rest = Vec::with_capacity(256);
            let _ = pipe_read.read_to_end(&mut rest);
            let _ = nix::sys::wait::waitpid(child, None);
            let mut full = Vec::with_capacity(1 + rest.len());
            full.push(first[0]);
            full.extend_from_slice(&rest);
            anyhow::bail!("{}", String::from_utf8_lossy(&full));
        }
        Ok(_) => {
            // EOF before any byte: child died silently. Capture exit
            // status so the message distinguishes panic vs. exit code
            // vs. signal kill (SIGILL etc.). Hint at the log file.
            let exit_info = match nix::sys::wait::waitpid(child, None) {
                Ok(nix::sys::wait::WaitStatus::Exited(_, code)) => format!("exit {code}"),
                Ok(nix::sys::wait::WaitStatus::Signaled(_, sig, _)) => {
                    format!("killed by signal {sig:?}")
                }
                Ok(other) => format!("{other:?}"),
                Err(e) => format!("waitpid: {e}"),
            };
            let log_hint = log_path
                .map(|p| format!("; check {} for tracing output", p.display()))
                .unwrap_or_default();
            anyhow::bail!("daemon exited before mount was ready ({exit_info}){log_hint}");
        }
        Err(e) => {
            anyhow::bail!("waiting for daemon: {e}");
        }
    }
}

fn install_signal_handler(mountpoint: String) {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    let triggered = Arc::new(AtomicBool::new(false));
    std::thread::spawn(move || {
        let mut signals = signal_hook::iterator::Signals::new([libc::SIGINT, libc::SIGTERM])
            .expect("failed to register signal handler");
        for sig in signals.forever() {
            if triggered.swap(true, Ordering::SeqCst) {
                // Already unmounting from a prior signal. Absorb the signal so the
                // process doesn't fall back to default disposition (which for SIGINT
                // would kill us before destroy() flushes dirty writes).
                eprintln!("veda: received signal {sig} again, unmount already in progress...");
                continue;
            }
            eprintln!("\nveda: received signal {sig}, unmounting {mountpoint}...");
            // Trigger graceful unmount: fuser::mount2's FUSE loop exits, mount2
            // returns, VedaFs::destroy runs, dirty write handles flush.
            //
            // NOTE: on macOS `umount` blocks until the FS is idle; in practice
            // fuser handles the unmount through the FUSE protocol so this
            // returns quickly. On Linux, fusermount3 -u is asynchronous.
            let argv = umount_argv(&mountpoint);
            // argv is non-empty by construction (see umount_argv_returns_nonempty).
            let _ = Command::new(&argv[0]).args(&argv[1..]).status();

            // Watchdog: if mount2 hasn't returned in 5s, force-exit so we don't
            // zombie forever on a wedged FS. Data may still be lost in that case.
            // This thread is killed automatically if the main thread exits first
            // (the normal path when mount2 returns cleanly).
            std::thread::spawn(|| {
                std::thread::sleep(std::time::Duration::from_secs(5));
                eprintln!("veda: unmount watchdog timeout, forcing exit");
                std::process::exit(1);
            });
            // Continue the for-loop: a second signal now lands in the `triggered`
            // branch above and is absorbed rather than killing the process.
        }
    });
}

fn cmd_umount(args: UmountArgs) -> anyhow::Result<()> {
    let argv = umount_argv(&args.mountpoint);
    let status = Command::new(&argv[0]).args(&argv[1..]).status()?;
    if !status.success() {
        anyhow::bail!("umount failed with {status}");
    }
    Ok(())
}

fn umount_argv(mountpoint: &str) -> Vec<String> {
    if cfg!(target_os = "macos") {
        return vec!["umount".into(), mountpoint.into()];
    }
    for bin in ["fusermount3", "fusermount"] {
        if which_exists(bin) {
            return vec![bin.into(), "-u".into(), mountpoint.into()];
        }
    }
    vec!["umount".into(), mountpoint.into()]
}

fn which_exists(name: &str) -> bool {
    Command::new("which")
        .arg(name)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn init_tracing(debug: bool) {
    let default_level = if debug { "debug" } else { "info" };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default_level)),
        )
        .init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn umount_argv_macos_uses_umount() {
        if cfg!(target_os = "macos") {
            let argv = umount_argv("/mnt/test");
            assert_eq!(argv, vec!["umount", "/mnt/test"]);
        }
    }

    #[test]
    fn umount_argv_returns_nonempty() {
        let argv = umount_argv("/mnt/test");
        assert!(!argv.is_empty());
        assert!(argv.last().unwrap() == "/mnt/test");
    }

    #[test]
    fn graceful_unmount_argv_uses_correct_binary_per_platform() {
        let argv = umount_argv("/tmp/veda-test-xyz123");
        if cfg!(target_os = "macos") {
            assert_eq!(argv[0], "umount", "macOS should use umount(8)");
        } else {
            // Linux: prefers fusermount3 or fusermount when available, else umount.
            let bin0 = &argv[0];
            assert!(
                bin0 == "fusermount3" || bin0 == "fusermount" || bin0 == "umount",
                "unexpected umount binary on Linux: {bin0}"
            );
        }
    }
}
