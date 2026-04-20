mod client;
mod fs;
mod inode;

use clap::Parser;
use tracing::info;

#[derive(Parser)]
#[command(name = "veda-fuse", about = "Mount a Veda workspace as a local filesystem")]
struct Args {
    #[arg(long, env = "VEDA_SERVER")]
    server: String,

    #[arg(long, env = "VEDA_KEY")]
    key: String,

    #[arg(long)]
    mount: String,

    #[arg(long, default_value = "false")]
    foreground: bool,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();

    let client = client::VedaClient::new(&args.server, &args.key);

    client.stat("/").map_err(|e| {
        anyhow::anyhow!("server health check failed (stat '/' at {}): {e}", args.server)
    })?;

    info!(mount = %args.mount, server = %args.server, "mounting veda workspace");

    let vedafs = fs::VedaFs::new(client);

    let mut options = vec![
        fuser::MountOption::FSName("veda".to_string()),
        fuser::MountOption::DefaultPermissions,
    ];
    if args.foreground {
        options.push(fuser::MountOption::AutoUnmount);
    }

    fuser::mount2(vedafs, &args.mount, &options)?;

    Ok(())
}
