mod cache;
mod client;
mod fs;
mod inode;
mod sse;

use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::process::Command;

use clap::{Parser, Subcommand};
use tracing::info;

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

    /// Attribute cache TTL in seconds
    #[arg(long, default_value = "5")]
    attr_ttl: u64,

    /// Allow other users to access the mount
    #[arg(long, default_value = "false")]
    allow_other: bool,

    /// Mount as read-only
    #[arg(long, default_value = "false")]
    read_only: bool,

    /// Enable FUSE debug logging
    #[arg(long, default_value = "false")]
    debug: bool,
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
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Cmd::Mount(args) => cmd_mount(args),
        Cmd::Umount(args) => cmd_umount(args),
    }
}

fn cmd_mount(args: MountArgs) -> anyhow::Result<()> {
    let client = client::VedaClient::new(&args.server, &args.key);

    client.stat("/").map_err(|e| {
        anyhow::anyhow!("server health check failed (stat '/' at {}): {e}", args.server)
    })?;

    let config = fs::FuseConfig {
        attr_ttl: Duration::from_secs(args.attr_ttl),
        read_only: args.read_only,
        cache_size_mb: args.cache_size,
    };

    let mount_opts = MountOpts {
        foreground: args.foreground,
        allow_other: args.allow_other,
        read_only: args.read_only,
    };

    if args.foreground {
        init_tracing(args.debug);
        info!(mount = %args.mountpoint, server = %args.server, "mounting (foreground)");
        return mount_and_serve(client, &args.mountpoint, &args.server, &args.key, config, &mount_opts);
    }

    // Daemonize: parent waits for child to signal readiness via pipe.
    let (read_fd, write_fd) = nix::unistd::pipe()?;
    let read_raw = read_fd.as_raw_fd();
    let write_raw = write_fd.as_raw_fd();

    match unsafe { nix::unistd::fork() }? {
        nix::unistd::ForkResult::Parent { child } => {
            drop(write_fd);
            wait_for_child_ready(read_raw, child)
        }
        nix::unistd::ForkResult::Child => {
            drop(read_fd);
            nix::unistd::setsid()?;

            init_tracing(args.debug);
            info!(mount = %args.mountpoint, server = %args.server, "mounting (background)");

            let result = mount_and_serve_with_notify(client, &args.mountpoint, &args.server, &args.key, config, &mount_opts, write_raw);
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
    client: client::VedaClient,
    mountpoint: &str,
    server: &str,
    key: &str,
    config: fs::FuseConfig,
    opts: &MountOpts,
) -> anyhow::Result<()> {
    let vedafs = fs::VedaFs::new(client, config, None);
    let mut watcher = sse::SseWatcher::start(
        server, key,
        vedafs.inodes(), vedafs.read_cache(),
    );
    let options = build_fuse_options(opts);
    install_signal_handler();
    let result = fuser::mount2(vedafs, mountpoint, &options);
    watcher.stop();
    result.map_err(Into::into)
}

fn mount_and_serve_with_notify(
    client: client::VedaClient,
    mountpoint: &str,
    server: &str,
    key: &str,
    config: fs::FuseConfig,
    opts: &MountOpts,
    notify_fd: RawFd,
) -> anyhow::Result<()> {
    let vedafs = fs::VedaFs::new(client, config, Some(notify_fd));
    let mut watcher = sse::SseWatcher::start(
        server, key,
        vedafs.inodes(), vedafs.read_cache(),
    );
    let options = build_fuse_options(opts);
    install_signal_handler();
    let result = fuser::mount2(vedafs, mountpoint, &options);
    watcher.stop();
    result.map_err(Into::into)
}

fn wait_for_child_ready(read_fd: RawFd, child: nix::unistd::Pid) -> anyhow::Result<()> {
    use std::io::Read;
    let mut pipe_read = unsafe { std::fs::File::from_raw_fd(read_fd) };
    let mut buf = [0u8; 1];

    match pipe_read.read(&mut buf) {
        Ok(1) if buf[0] == b'R' => {
            eprintln!("veda: mounted (pid {})", child);
            Ok(())
        }
        Ok(_) => {
            anyhow::bail!("daemon exited before mount was ready");
        }
        Err(e) => {
            anyhow::bail!("waiting for daemon: {e}");
        }
    }
}

fn install_signal_handler() {
    std::thread::spawn(|| {
        let mut signals = signal_hook::iterator::Signals::new([libc::SIGINT, libc::SIGTERM])
            .expect("failed to register signal handler");
        for sig in signals.forever() {
            eprintln!("\nveda: received signal {sig}, unmounting...");
            std::process::exit(0);
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
}
