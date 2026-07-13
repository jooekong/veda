//! One WeCom bot = one long connection = one task.
//!
//! Lifecycle (§7.1): connect WSS → subscribe → on ok-ack mark `Subscribed`
//! → 30s heartbeat → on disconnect/kick/error mark `Reconnecting` and
//! exponentially back off → reconnect. A `shutdown` watch flipping to true
//! stops the task for good (used by both reconnect — main respawns — and
//! full stop).
//!
//! Concurrency shape: the WS sink is owned by a single writer task fed by an
//! `mpsc<Value>` channel; the read loop, heartbeat, and per-message handlers
//! all push frames through that channel, so the sink is never shared.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context as _};
use futures_util::{SinkExt, StreamExt};
use moka::future::Cache;
use serde_json::Value;
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};

use crate::config::BotConfig;
use super::handler::{handle_message, truncate, HandlerCtx};
use crate::registry::{self, ConnState, Registry};
use crate::veda::VedaClient;
use crate::wecom::protocol::{
    ping_frame, subscribe_frame, EventCallbackBody, MsgCallbackBody, RawFrame, CMD_EVENT_CALLBACK,
    CMD_MSG_CALLBACK, EVENT_DISCONNECTED,
};

const WECOM_WS_URL: &str = "wss://openws.work.weixin.qq.com";
const HEARTBEAT_SECS: u64 = 30;
const DEDUP_TTL_SECS: u64 = 600;
const OUTBOUND_CAP: usize = 64;
const MAX_BACKOFF_SECS: u64 = 30;
const HEALTHY_CONN_SECS: u64 = 5;
const CONNECT_TIMEOUT_SECS: u64 = 15;

/// Per-bot immutable runtime handles.
pub struct BotRuntime {
    pub bot: Arc<BotConfig>,
    pub veda: Arc<VedaClient>,
    pub registry: Registry,
    /// Global answer switch, forwarded to every handler ctx.
    pub answer_enabled: bool,
}

fn new_req_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// True when this bot task must stop: the flag flipped, OR the sender side
/// was dropped (a `bots.insert` overwriting an old handle drops its sender
/// without sending). Treating closed-as-shutdown is what prevents an orphaned
/// task from reconnecting forever and kick-warring its replacement.
fn stop_requested(shutdown: &watch::Receiver<bool>) -> bool {
    *shutdown.borrow() || shutdown.has_changed().is_err()
}

/// Drive one bot until `shutdown` flips to true (or its sender is dropped).
pub async fn run_bot(rt: BotRuntime, mut shutdown: watch::Receiver<bool>) {
    // msgid dedup: WeCom re-pushes a message if it doesn't see our 5s ack.
    // Per-bot cache; the read loop is single-threaded so contains+insert is
    // race-free.
    let dedup: Cache<String, ()> = Cache::builder()
        .time_to_live(Duration::from_secs(DEDUP_TTL_SECS))
        .max_capacity(10_000)
        .build();

    let mut backoff = Duration::from_secs(1);

    while !stop_requested(&shutdown) {
        registry::update(&rt.registry, &rt.bot.bot_id, |s| {
            s.conn_state = ConnState::Connecting;
        });

        let started = std::time::Instant::now();
        let outcome = serve_once(&rt, &dedup, &mut shutdown).await;

        if stop_requested(&shutdown) {
            break;
        }

        match outcome {
            Ok(()) => info!(bot = %rt.bot.name, "connection ended, reconnecting"),
            Err(e) => {
                warn!(bot = %rt.bot.name, error = %e, "connection error, reconnecting");
                registry::update(&rt.registry, &rt.bot.bot_id, |s| {
                    s.last_error = Some(truncate(&e.to_string(), 200));
                });
            }
        }
        registry::update(&rt.registry, &rt.bot.bot_id, |s| {
            s.conn_state = ConnState::Reconnecting;
            s.connected_since = None;
        });

        // Connection that survived a while → healthy; reset backoff.
        if started.elapsed() > Duration::from_secs(HEALTHY_CONN_SECS) {
            backoff = Duration::from_secs(1);
        }

        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            _ = shutdown.changed() => {}
        }
        backoff = (backoff * 2).min(Duration::from_secs(MAX_BACKOFF_SECS));
    }

    registry::update(&rt.registry, &rt.bot.bot_id, |s| {
        s.conn_state = ConnState::Down;
        s.connected_since = None;
    });
    info!(bot = %rt.bot.name, "bot task stopped");
}

/// One connect → subscribe → serve cycle. Returns `Ok` on graceful peer
/// close, `Err` on any fault the caller should reconnect from.
async fn serve_once(
    rt: &BotRuntime,
    dedup: &Cache<String, ()>,
    shutdown: &mut watch::Receiver<bool>,
) -> anyhow::Result<()> {
    // The handshake must stay interruptible: without the select, a task stuck
    // in TCP/TLS connect ignores shutdown past stop_bot's grace window and can
    // still subscribe AFTER its replacement — a kick-war. The timeout bounds a
    // black-holed connect (tungstenite has none of its own).
    let (ws, _) = tokio::select! {
        r = tokio::time::timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS), connect_async(WECOM_WS_URL)) => {
            r.context("ws connect timed out")?.context("ws connect")?
        }
        _ = shutdown.changed() => return Ok(()),
    };
    let (mut write, mut read) = ws.split();

    let (out_tx, mut out_rx) = mpsc::channel::<Value>(OUTBOUND_CAP);

    // Writer task owns the sink.
    let writer = tokio::spawn(async move {
        while let Some(v) = out_rx.recv().await {
            if write.send(Message::Text(v.to_string().into())).await.is_err() {
                break;
            }
        }
        let _ = write.close().await;
    });

    // Subscribe (first frame).
    let _ = out_tx
        .send(subscribe_frame(&new_req_id(), &rt.bot.bot_id, &rt.bot.secret))
        .await;

    // Heartbeat task.
    let hb_tx = out_tx.clone();
    let heartbeat = tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(HEARTBEAT_SECS));
        tick.tick().await; // consume the immediate first tick
        loop {
            tick.tick().await;
            if hb_tx.send(ping_frame(&new_req_id())).await.is_err() {
                break;
            }
        }
    });

    let result = read_loop(rt, dedup, &mut read, &out_tx, shutdown).await;

    heartbeat.abort();
    drop(out_tx); // closing the channel ends the writer task
    let _ = writer.await;
    result
}

async fn read_loop(
    rt: &BotRuntime,
    dedup: &Cache<String, ()>,
    read: &mut (impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin),
    out_tx: &mpsc::Sender<Value>,
    shutdown: &mut watch::Receiver<bool>,
) -> anyhow::Result<()> {
    loop {
        tokio::select! {
            _ = shutdown.changed() => return Ok(()),
            frame = read.next() => {
                let msg = match frame {
                    Some(Ok(m)) => m,
                    Some(Err(e)) => return Err(anyhow!("ws read: {e}")),
                    None => return Err(anyhow!("connection closed by peer")),
                };
                match msg {
                    Message::Text(txt) => {
                        if handle_frame(rt, dedup, txt.as_str(), out_tx).await {
                            return Err(anyhow!("kicked by new connection"));
                        }
                    }
                    Message::Close(_) => return Err(anyhow!("server sent close")),
                    // Ping/Pong are handled by tungstenite; app-level ping is
                    // the `ping` cmd, not a WS control frame.
                    _ => {}
                }
            }
        }
    }
}

/// Dispatch one inbound text frame. Returns `true` when we've been kicked
/// (`disconnected_event`) and the caller should reconnect.
async fn handle_frame(
    rt: &BotRuntime,
    dedup: &Cache<String, ()>,
    txt: &str,
    out_tx: &mpsc::Sender<Value>,
) -> bool {
    let raw: RawFrame = match serde_json::from_str(txt) {
        Ok(r) => r,
        Err(e) => {
            debug!(bot = %rt.bot.name, error = %e, "drop unparseable frame");
            return false;
        }
    };

    match raw.cmd.as_deref() {
        // No cmd → subscribe/ping ACK.
        None => {
            if raw.is_ok_ack() {
                registry::update(&rt.registry, &rt.bot.bot_id, |s| {
                    if s.conn_state != ConnState::Subscribed {
                        s.conn_state = ConnState::Subscribed;
                        s.connected_since = Some(chrono::Utc::now());
                        s.last_error = None;
                    }
                });
                debug!(bot = %rt.bot.name, "ack ok");
            } else if raw.errcode.unwrap_or(0) != 0 {
                warn!(bot = %rt.bot.name, errcode = ?raw.errcode, errmsg = ?raw.errmsg, "ack error");
                registry::update(&rt.registry, &rt.bot.bot_id, |s| {
                    s.last_error = Some(format!("ack errcode {:?}", raw.errcode));
                });
            }
            false
        }
        Some(CMD_MSG_CALLBACK) => {
            let Some(body_val) = raw.body else {
                return false;
            };
            let body: MsgCallbackBody = match serde_json::from_value(body_val) {
                Ok(b) => b,
                Err(e) => {
                    warn!(bot = %rt.bot.name, error = %e, "bad msg_callback body");
                    return false;
                }
            };
            // Dedup BEFORE spawning: mark on receipt so a 5s-retry re-push
            // can't double-search. Safe without a lock — single read loop.
            if dedup.contains_key(&body.msgid) {
                debug!(bot = %rt.bot.name, msgid = %body.msgid, "dropped duplicate");
                return false;
            }
            dedup.insert(body.msgid.clone(), ()).await;

            let ctx = HandlerCtx {
                bot: rt.bot.clone(),
                veda: rt.veda.clone(),
                registry: rt.registry.clone(),
                outbound: out_tx.clone(),
                answer_enabled: rt.answer_enabled,
            };
            tokio::spawn(handle_message(ctx, raw.headers.req_id, body));
            false
        }
        Some(CMD_EVENT_CALLBACK) => {
            let kicked = raw
                .body
                .and_then(|b| serde_json::from_value::<EventCallbackBody>(b).ok())
                .and_then(|e| e.event)
                .map(|ev| ev.eventtype == EVENT_DISCONNECTED)
                .unwrap_or(false);
            if kicked {
                info!(bot = %rt.bot.name, "received disconnected_event (kicked)");
            }
            kicked
        }
        Some(other) => {
            debug!(bot = %rt.bot.name, cmd = other, "unhandled frame");
            false
        }
    }
}
