//! Message handling: dedup happens upstream in the read loop; here we strip
//! the `@mention`, search, render markdown, and stream the reply back.
//!
//! Each inbound question is handled in its own spawned task so a slow search
//! never blocks the connection's read loop (heartbeats, other messages).
//! The reply is pushed through the `outbound` channel that the connection's
//! writer task drains — the WS sink is never touched from here.

use std::sync::Arc;

use chrono::Utc;
use serde_json::Value;
use tokio::sync::mpsc;
use tracing::{info, warn};

use super::protocol::{respond_stream_frame, MsgCallbackBody, MSGTYPE_TEXT};
use crate::config::BotConfig;
use crate::registry::{self, Registry};
use crate::veda::{Hit, SearchError, VedaClient};

/// Everything a handler task needs. All fields are cheap to clone (Arc /
/// channel sender / Arc-backed registry).
#[derive(Clone)]
pub struct HandlerCtx {
    pub bot: Arc<BotConfig>,
    pub veda: Arc<VedaClient>,
    pub registry: Registry,
    pub outbound: mpsc::Sender<Value>,
}

pub async fn handle_message(ctx: HandlerCtx, req_id: String, body: MsgCallbackBody) {
    // One stream id per question; reused across the placeholder + final frame
    // so WeCom refreshes the same bubble.
    let stream_id = uuid::Uuid::new_v4().to_string();

    if body.msgtype.as_deref() != Some(MSGTYPE_TEXT) {
        send_final(&ctx, &req_id, &stream_id, "暂只支持文字提问").await;
        return;
    }

    let raw_text = body.text.map(|t| t.content).unwrap_or_default();
    let query = strip_mention(&raw_text);
    if query.is_empty() {
        send_final(&ctx, &req_id, &stream_id, "请在 @ 机器人后输入要查询的问题").await;
        return;
    }

    // Placeholder frame — must reach WeCom within 5s; search may take longer.
    let _ = ctx
        .outbound
        .send(respond_stream_frame(
            &req_id,
            &stream_id,
            "正在查阅知识库…",
            false,
        ))
        .await;

    registry::update(&ctx.registry, &ctx.bot.bot_id, |s| {
        s.last_msg_at = Some(Utc::now());
        s.msg_count += 1;
    });

    let started = std::time::Instant::now();
    let reply = match ctx
        .veda
        .search(&ctx.bot.veda_key, &query, &ctx.bot.mode, ctx.bot.limit)
        .await
    {
        Ok(hits) => {
            info!(
                bot = %ctx.bot.name,
                query = %query,
                hits = hits.len(),
                ms = started.elapsed().as_millis() as u64,
                "search ok"
            );
            render_markdown(&hits)
        }
        Err(SearchError::Unauthorized) => {
            registry::update(&ctx.registry, &ctx.bot.bot_id, |s| {
                s.error_count += 1;
                s.last_error = Some("search 401 (key invalid/revoked)".to_string());
            });
            // Keep the WeCom connection alive — a bad key is not a reason to
            // drop the long connection (§9).
            warn!(bot = %ctx.bot.name, "search unauthorized (401)");
            "知识库鉴权失败，请联系管理员".to_string()
        }
        Err(SearchError::Unavailable(e)) => {
            registry::update(&ctx.registry, &ctx.bot.bot_id, |s| {
                s.error_count += 1;
                s.last_error = Some(truncate(&e, 200));
            });
            warn!(bot = %ctx.bot.name, error = %e, "search unavailable");
            "知识库暂时不可用，请稍后再试".to_string()
        }
    };

    send_final(&ctx, &req_id, &stream_id, &reply).await;
}

async fn send_final(ctx: &HandlerCtx, req_id: &str, stream_id: &str, content: &str) {
    let _ = ctx
        .outbound
        .send(respond_stream_frame(req_id, stream_id, content, true))
        .await;
}

/// Group text arrives with the mention inline as plain text, e.g.
/// `"@RobotName 报销流程"` — there is no structured mention offset in text
/// frames, so we drop a single leading `@token` (up to the first
/// whitespace) and keep the rest.
fn strip_mention(s: &str) -> String {
    let t = s.trim_start();
    match t.strip_prefix('@') {
        Some(rest) => match rest.find(char::is_whitespace) {
            Some(i) => rest[i..].trim().to_string(),
            None => String::new(), // only a mention, no question
        },
        None => t.trim().to_string(),
    }
}

fn render_markdown(hits: &[Hit]) -> String {
    if hits.is_empty() {
        return "没找到相关内容".to_string();
    }
    let mut out = String::new();
    for (i, h) in hits.iter().enumerate() {
        let snippet = truncate(h.content.trim(), 300);
        let src = h.path.as_deref().unwrap_or("未知");
        out.push_str(&format!("**{}.** {}\n出处：`{}`\n\n", i + 1, snippet, src));
    }
    out.trim_end().to_string()
}

/// Char-based truncation (never splits a multi-byte UTF-8 boundary).
pub(crate) fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let t: String = s.chars().take(max).collect();
        format!("{t}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_leading_mention() {
        assert_eq!(strip_mention("@Robot 报销流程"), "报销流程");
        assert_eq!(strip_mention("  @Robot   多空格问题  "), "多空格问题");
    }

    #[test]
    fn no_mention_keeps_text() {
        assert_eq!(strip_mention("直接单聊提问"), "直接单聊提问");
    }

    #[test]
    fn mention_only_is_empty() {
        assert_eq!(strip_mention("@Robot"), "");
    }

    #[test]
    fn empty_hits_render_not_found() {
        assert_eq!(render_markdown(&[]), "没找到相关内容");
    }

    #[test]
    fn renders_hit_with_source() {
        let hits = vec![Hit {
            content: "报销需在 OA 提交".to_string(),
            path: Some("/hr/报销.md".to_string()),
            score: 0.9,
            score_type: Some("rrf".to_string()),
        }];
        let md = render_markdown(&hits);
        assert!(md.contains("报销需在 OA 提交"));
        assert!(md.contains("/hr/报销.md"));
    }

    #[test]
    fn missing_path_renders_unknown() {
        let hits = vec![Hit {
            content: "x".to_string(),
            path: None,
            score: 0.0,
            score_type: None,
        }];
        assert!(render_markdown(&hits).contains("未知"));
    }

    #[test]
    fn truncate_multibyte_safe() {
        assert_eq!(truncate("你好世界", 2), "你好…");
        assert_eq!(truncate("abc", 5), "abc");
    }
}
