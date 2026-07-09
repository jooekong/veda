//! veda-tunnel entrypoint: connect MySQL bot store → spawn one task per bot
//! → run the admin server + a control loop until Ctrl-C. Single instance by
//! design (the WeCom single-connection rule; see docs/plans/veda-tunnel-plan.md
//! §12). The store (`veda_tunnel_bots`) is the source of truth; the control
//! loop keeps the running fleet in sync with it.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use veda_tunnel::admin::{self, AdminState, ControlCmd};
use veda_tunnel::config::{BotConfig, TunnelConfig};
use veda_tunnel::registry::{self, Registry};
use veda_tunnel::store::BotStore;
use veda_tunnel::veda::VedaClient;
use veda_tunnel::wecom::conn::{run_bot, BotRuntime};

/// Handle to one running bot task: its shutdown switch + join handle.
struct BotHandle {
    shutdown: watch::Sender<bool>,
    join: JoinHandle<()>,
}

fn spawn_bot(bot: BotConfig, veda: Arc<VedaClient>, reg: Registry) -> BotHandle {
    let (sd_tx, sd_rx) = watch::channel(false);
    let rt = BotRuntime {
        bot: Arc::new(bot),
        veda,
        registry: reg,
    };
    let join = tokio::spawn(run_bot(rt, sd_rx));
    BotHandle {
        shutdown: sd_tx,
        join,
    }
}

async fn stop_bot(h: BotHandle) {
    let _ = h.shutdown.send(true);
    if tokio::time::timeout(Duration::from_secs(10), h.join)
        .await
        .is_err()
    {
        warn!("bot task did not stop within 10s");
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    // tokio-tungstenite's rustls path needs a process-level default crypto
    // provider before the first WSS handshake (rustls 0.23; the dep tree has
    // both aws-lc-rs and ring, so it can't auto-select). Match reqwest's
    // provider (aws-lc-rs). Ignore Err — it only means one is already set.
    if rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .is_err()
    {
        tracing::debug!("rustls crypto provider already installed");
    }

    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config/tunnel.toml".to_string());
    let cfg = TunnelConfig::load(&config_path)?;
    info!(
        veda = %cfg.veda_base_url,
        admin = %cfg.admin.listen,
        "starting veda-tunnel"
    );

    let veda = Arc::new(VedaClient::new(cfg.veda_base_url.clone())?);
    let store = Arc::new(BotStore::connect(&cfg.mysql.database_url).await?);

    // First-run seed: import tunnel.toml's [[wecom.bot]] into an empty store.
    if store.count().await? == 0 && !cfg.wecom.bot.is_empty() {
        info!(
            n = cfg.wecom.bot.len(),
            "seeding bots from tunnel.toml into empty store"
        );
        for b in &cfg.wecom.bot {
            if let Err(e) = store.add(b).await {
                warn!(bot = %b.name, error = %e, "seed add failed");
            }
        }
    }

    let bot_cfgs = store.list().await?;
    let reg = registry::seed(&bot_cfgs);
    let mut bots: HashMap<String, BotHandle> = HashMap::new();
    for b in &bot_cfgs {
        bots.insert(
            b.bot_id.clone(),
            spawn_bot(b.clone(), veda.clone(), reg.clone()),
        );
    }
    info!(bots = bot_cfgs.len(), "bots spawned from store");

    let (control_tx, mut control_rx) = mpsc::channel::<ControlCmd>(16);

    // Admin server (detached; process exit tears it down).
    let admin_state = AdminState {
        registry: reg.clone(),
        store: store.clone(),
        admin_token: cfg.admin.token.clone(),
        control: control_tx.clone(),
    };
    let listener = TcpListener::bind(&cfg.admin.listen).await?;
    info!(listen = %cfg.admin.listen, "admin surface up");
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, admin::router(admin_state)).await {
            error!(error = %e, "admin server exited");
        }
    });

    // Control loop: react to Ctrl-C and admin intents.
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("ctrl-c received, shutting down");
                break;
            }
            Some(cmd) = control_rx.recv() => {
                handle_control(cmd, &store, &veda, &reg, &mut bots).await;
            }
        }
    }

    for (_, h) in bots.drain() {
        stop_bot(h).await;
    }
    info!("veda-tunnel stopped");
    Ok(())
}

/// Apply an admin command: mutate the store, then reflect it onto live bot
/// tasks. The store is the source of truth; a task change only happens after
/// its DB write succeeds.
async fn handle_control(
    cmd: ControlCmd,
    store: &Arc<BotStore>,
    veda: &Arc<VedaClient>,
    reg: &Registry,
    bots: &mut HashMap<String, BotHandle>,
) {
    match cmd {
        ControlCmd::AddBot { bot, reply } => match store.add(&bot).await {
            Ok(()) => {
                registry::insert(reg, &bot);
                bots.insert(
                    bot.bot_id.clone(),
                    spawn_bot(bot, veda.clone(), reg.clone()),
                );
                let _ = reply.send(Ok(()));
            }
            Err(e) => {
                let _ = reply.send(Err(e.to_string()));
            }
        },
        ControlCmd::UpdateBot { bot, reply } => match store.update(&bot).await {
            Ok(true) => {
                // Re-fetch the canonical row: the update may have kept the
                // stored secret (empty in the request), which spawn needs.
                match store.get(&bot.bot_id).await {
                    Ok(Some(full)) => {
                        if let Some(h) = bots.remove(&full.bot_id) {
                            stop_bot(h).await;
                        }
                        registry::insert(reg, &full);
                        bots.insert(
                            full.bot_id.clone(),
                            spawn_bot(full, veda.clone(), reg.clone()),
                        );
                        let _ = reply.send(Ok(()));
                    }
                    _ => {
                        let _ = reply.send(Err("reload after update failed".to_string()));
                    }
                }
            }
            Ok(false) => {
                let _ = reply.send(Err("unknown bot_id".to_string()));
            }
            Err(e) => {
                let _ = reply.send(Err(e.to_string()));
            }
        },
        ControlCmd::RemoveBot { bot_id, reply } => match store.remove(&bot_id).await {
            Ok(true) => {
                if let Some(h) = bots.remove(&bot_id) {
                    stop_bot(h).await;
                }
                registry::remove(reg, &bot_id);
                let _ = reply.send(true);
            }
            Ok(false) => {
                let _ = reply.send(false);
            }
            Err(e) => {
                warn!(error = %e, "remove bot db error");
                let _ = reply.send(false);
            }
        },
        ControlCmd::Reconnect { bot_id, reply } => {
            let Some(h) = bots.remove(&bot_id) else {
                let _ = reply.send(false);
                return;
            };
            stop_bot(h).await;
            match store.get(&bot_id).await {
                Ok(Some(bot)) => {
                    bots.insert(
                        bot_id.clone(),
                        spawn_bot(bot, veda.clone(), reg.clone()),
                    );
                    info!(bot_id = %bot_id, "reconnect: respawned");
                    let _ = reply.send(true);
                }
                _ => {
                    let _ = reply.send(false);
                }
            }
        }
        ControlCmd::Reload { reply } => {
            // Re-read the whole fleet from the store (e.g. after an external
            // DB edit) and rebuild every connection.
            let bot_cfgs = match store.list().await {
                Ok(b) => b,
                Err(e) => {
                    let _ = reply.send(Err(e.to_string()));
                    return;
                }
            };
            for (_, h) in bots.drain() {
                stop_bot(h).await;
            }
            registry::reseed(reg, &bot_cfgs);
            for b in &bot_cfgs {
                bots.insert(
                    b.bot_id.clone(),
                    spawn_bot(b.clone(), veda.clone(), reg.clone()),
                );
            }
            let n = bot_cfgs.len();
            info!(bots = n, "reload: complete");
            let _ = reply.send(Ok(n));
        }
    }
}
