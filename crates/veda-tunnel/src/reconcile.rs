//! Store-poll reconciliation: every 30s main diffs the MySQL bot table
//! (desired) against the running fleet (actual) and converges. This is how
//! bots created/edited/deleted by veda-server's platform API — which writes
//! the shared table directly, bypassing tunnel's admin API — take effect
//! without a restart. The diff itself is a pure function so it can be unit
//! tested; main executes the returned actions.

use std::collections::HashMap;
use std::sync::Arc;

use crate::config::BotConfig;

#[derive(Debug, PartialEq)]
pub enum Action {
    /// New bot in the store — spawn a connection.
    Spawn(BotConfig),
    /// Config changed (any field) — stop the old task, spawn with the new.
    Respawn(BotConfig),
    /// Row gone from the store — stop and drop the task.
    Stop(String),
}

/// Diff desired (store rows) vs running (bot_id → active config).
/// Order: stops first so a bot_id reused across delete+add (same tick)
/// releases its WeCom connection before the new spawn takes it.
pub fn plan(desired: &[BotConfig], running: &HashMap<String, Arc<BotConfig>>) -> Vec<Action> {
    let desired_ids: HashMap<&str, &BotConfig> =
        desired.iter().map(|b| (b.bot_id.as_str(), b)).collect();
    let mut actions: Vec<Action> = running
        .keys()
        .filter(|id| !desired_ids.contains_key(id.as_str()))
        .map(|id| Action::Stop(id.clone()))
        .collect();
    for b in desired {
        match running.get(&b.bot_id) {
            None => actions.push(Action::Spawn(b.clone())),
            Some(cur) if cur.as_ref() != b => actions.push(Action::Respawn(b.clone())),
            Some(_) => {}
        }
    }
    actions
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bot(id: &str, name: &str, limit: usize) -> BotConfig {
        BotConfig {
            name: name.to_string(),
            bot_id: id.to_string(),
            secret: "s".to_string(),
            veda_key: "wk_x".to_string(),
            workspace: "ws".to_string(),
            project: None,
            mode: "hybrid".to_string(),
            limit,
            prompt: None,
        }
    }

    #[test]
    fn prompt_change_respawns() {
        let mut d = vec![bot("a", "a", 8)];
        let r = running(&d);
        d[0].prompt = Some("# 角色\n新 persona".to_string());
        let acts = plan(&d, &r);
        assert!(matches!(acts.as_slice(), [Action::Respawn(b)] if b.bot_id == "a"));
    }

    fn running(bots: &[BotConfig]) -> HashMap<String, Arc<BotConfig>> {
        bots.iter()
            .map(|b| (b.bot_id.clone(), Arc::new(b.clone())))
            .collect()
    }

    #[test]
    fn no_change_is_empty() {
        let d = vec![bot("a", "a", 8)];
        assert!(plan(&d, &running(&d)).is_empty());
    }

    #[test]
    fn new_bot_spawns() {
        let d = vec![bot("a", "a", 8)];
        let acts = plan(&d, &running(&[]));
        assert_eq!(acts, vec![Action::Spawn(d[0].clone())]);
    }

    #[test]
    fn removed_bot_stops() {
        let acts = plan(&[], &running(&[bot("a", "a", 8)]));
        assert_eq!(acts, vec![Action::Stop("a".to_string())]);
    }

    #[test]
    fn changed_field_respawns() {
        let d = vec![bot("a", "a", 12)];
        let acts = plan(&d, &running(&[bot("a", "a", 8)]));
        assert_eq!(acts, vec![Action::Respawn(d[0].clone())]);
    }

    #[test]
    fn stops_come_before_spawns() {
        let d = vec![bot("b", "b", 8)];
        let acts = plan(&d, &running(&[bot("a", "a", 8)]));
        assert_eq!(
            acts,
            vec![Action::Stop("a".to_string()), Action::Spawn(d[0].clone())]
        );
    }
}
