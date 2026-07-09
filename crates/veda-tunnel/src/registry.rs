//! In-process bot registry — the source of truth for "what's running".
//!
//! Each connection task updates its own row; the admin surface reads
//! snapshots. Bot count is small and updates are infrequent (state
//! transitions + per-message counters), so a plain `RwLock<HashMap>` is
//! plenty — no need for dashmap's sharding.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::config::BotConfig;

pub type Registry = Arc<RwLock<HashMap<String, BotStatus>>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnState {
    Connecting,
    Subscribed,
    Reconnecting,
    Down,
}

#[derive(Debug, Clone, Serialize)]
pub struct BotStatus {
    pub name: String,
    pub bot_id: String,
    pub workspace: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    pub conn_state: ConnState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connected_since: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_msg_at: Option<DateTime<Utc>>,
    pub msg_count: u64,
    pub error_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

impl BotStatus {
    pub fn new(bot: &BotConfig) -> Self {
        Self {
            name: bot.name.clone(),
            bot_id: bot.bot_id.clone(),
            workspace: bot.workspace.clone(),
            project: bot.project.clone(),
            conn_state: ConnState::Connecting,
            connected_since: None,
            last_msg_at: None,
            msg_count: 0,
            error_count: 0,
            last_error: None,
        }
    }
}

/// Build a fresh registry seeded with one `Connecting` row per bot.
pub fn seed(bots: &[BotConfig]) -> Registry {
    let map = bots
        .iter()
        .map(|b| (b.bot_id.clone(), BotStatus::new(b)))
        .collect();
    Arc::new(RwLock::new(map))
}

/// Mutate one bot's row in place. No-op if the bot is gone (e.g. removed by
/// a concurrent reload) or the lock is poisoned.
pub fn update<F: FnOnce(&mut BotStatus)>(reg: &Registry, bot_id: &str, f: F) {
    if let Ok(mut map) = reg.write() {
        if let Some(s) = map.get_mut(bot_id) {
            f(s);
        }
    }
}

/// Snapshot all rows, sorted by name for stable admin output.
pub fn snapshot(reg: &Registry) -> Vec<BotStatus> {
    let mut v: Vec<BotStatus> = reg
        .read()
        .map(|m| m.values().cloned().collect())
        .unwrap_or_default();
    v.sort_by(|a, b| a.name.cmp(&b.name));
    v
}

pub fn get(reg: &Registry, bot_id: &str) -> Option<BotStatus> {
    reg.read().ok().and_then(|m| m.get(bot_id).cloned())
}

/// Replace all rows in place, keeping the same `Arc` — used by admin reload
/// so the admin surface's cloned handle keeps seeing live data.
pub fn reseed(reg: &Registry, bots: &[BotConfig]) {
    if let Ok(mut m) = reg.write() {
        m.clear();
        for b in bots {
            m.insert(b.bot_id.clone(), BotStatus::new(b));
        }
    }
}

/// Add one row (dynamic bot add).
pub fn insert(reg: &Registry, bot: &BotConfig) {
    if let Ok(mut m) = reg.write() {
        m.insert(bot.bot_id.clone(), BotStatus::new(bot));
    }
}

/// Drop one row (dynamic bot remove).
pub fn remove(reg: &Registry, bot_id: &str) {
    if let Ok(mut m) = reg.write() {
        m.remove(bot_id);
    }
}
