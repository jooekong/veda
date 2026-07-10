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
use crate::veda::{AnswerData, Hit, SearchError, VedaClient};

/// Everything a handler task needs. All fields are cheap to clone (Arc /
/// channel sender / Arc-backed registry).
#[derive(Clone)]
pub struct HandlerCtx {
    pub bot: Arc<BotConfig>,
    pub veda: Arc<VedaClient>,
    pub registry: Registry,
    pub outbound: mpsc::Sender<Value>,
    /// Global answer switch (`[answer] enabled`): true → route through
    /// `/v1/answer`, false → raw search. Process-wide, not per-bot.
    pub answer_enabled: bool,
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
    let reply = if ctx.answer_enabled {
        answer_reply(&ctx, &query, started).await
    } else {
        search_reply(&ctx, &query, started).await
    };

    send_final(&ctx, &req_id, &stream_id, &reply).await;
}

/// Pre-answer path: raw `/v1/search` + snippet rendering. Behaviour is
/// unchanged from before the answer switch existed.
async fn search_reply(ctx: &HandlerCtx, query: &str, started: std::time::Instant) -> String {
    match ctx
        .veda
        .search(&ctx.bot.veda_key, query, &ctx.bot.mode, ctx.bot.limit)
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
        // search() only maps 401→Unauthorized and everything else→Unavailable,
        // so these answer-only variants never reach here; treat them as
        // unavailable to keep the match exhaustive.
        Err(SearchError::Disabled) | Err(SearchError::Throttled) => {
            "知识库暂时不可用，请稍后再试".to_string()
        }
    }
}

/// Answer path: veda's `/v1/answer` RAG endpoint — body + a verifiable
/// citation list, with per-error fallbacks (§8). A failed call is recorded on
/// the bot's error counters, matching the search path.
async fn answer_reply(ctx: &HandlerCtx, query: &str, started: std::time::Instant) -> String {
    match ctx.veda.answer(&ctx.bot.veda_key, query).await {
        Ok(data) => {
            info!(
                bot = %ctx.bot.name,
                query = %query,
                citations = data.citations.len(),
                ms = started.elapsed().as_millis() as u64,
                "answer ok"
            );
            render_answer(&data)
        }
        Err(SearchError::Disabled) => {
            registry::update(&ctx.registry, &ctx.bot.bot_id, |s| {
                s.error_count += 1;
                s.last_error = Some("answer disabled (501)".to_string());
            });
            warn!(bot = %ctx.bot.name, "answer disabled (501)");
            "知识库问答未启用".to_string()
        }
        Err(SearchError::Throttled) => {
            registry::update(&ctx.registry, &ctx.bot.bot_id, |s| {
                s.error_count += 1;
                s.last_error = Some("answer throttled (429)".to_string());
            });
            warn!(bot = %ctx.bot.name, "answer throttled (429)");
            "提问太频繁，请稍后再试".to_string()
        }
        Err(SearchError::Unauthorized) => {
            registry::update(&ctx.registry, &ctx.bot.bot_id, |s| {
                s.error_count += 1;
                s.last_error = Some("answer 401 (key invalid/revoked)".to_string());
            });
            // Keep the WeCom connection alive — a bad key is not a reason to
            // drop the long connection (§9).
            warn!(bot = %ctx.bot.name, "answer unauthorized (401)");
            "知识库鉴权失败，请联系管理员".to_string()
        }
        Err(SearchError::Unavailable(e)) => {
            registry::update(&ctx.registry, &ctx.bot.bot_id, |s| {
                s.error_count += 1;
                s.last_error = Some(truncate(&e, 200));
            });
            warn!(bot = %ctx.bot.name, error = %e, "answer unavailable");
            "知识库暂时不可用，请稍后再试".to_string()
        }
    }
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

/// Render a `/v1/answer` result for WeCom: the answer body, then a separator
/// and a "出处：" list of `[n] path` lines. `[n]` reuses the server-assigned
/// citation index so it lines up with the `[n]` markers in the body.
/// Citations without a resolvable path are skipped; if none remain, only the
/// body is sent.
fn render_answer(data: &AnswerData) -> String {
    let body = data.answer.trim();
    let cited: Vec<(usize, &str)> = data
        .citations
        .iter()
        .filter_map(|c| c.path.as_deref().map(|p| (c.index, p)))
        .collect();
    if cited.is_empty() {
        return body.to_string();
    }
    let mut out = format!("{body}\n\n———\n出处：\n");
    for (idx, path) in cited {
        out.push_str(&format!("[{idx}] `{path}`\n"));
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
    use crate::veda::AnswerCitation;

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

    #[test]
    fn render_answer_with_citations() {
        let data = AnswerData {
            answer: "接入分三步[1]，多活需额外申请[2]".to_string(),
            citations: vec![
                AnswerCitation {
                    index: 1,
                    path: Some("/a/接入.md".to_string()),
                },
                AnswerCitation {
                    index: 2,
                    path: Some("/b/多活.md".to_string()),
                },
            ],
        };
        let out = render_answer(&data);
        assert!(out.contains("接入分三步[1]"));
        assert!(out.contains("———"));
        assert!(out.contains("出处："));
        assert!(out.contains("[1] `/a/接入.md`"));
        assert!(out.contains("[2] `/b/多活.md`"));
    }

    #[test]
    fn render_answer_no_citations_body_only() {
        let data = AnswerData {
            answer: "知识库中没有找到相关内容".to_string(),
            citations: vec![],
        };
        let out = render_answer(&data);
        assert_eq!(out, "知识库中没有找到相关内容");
        assert!(!out.contains("出处："));
    }

    #[test]
    fn render_answer_skips_none_path() {
        // Citation with index 1 has no path → dropped from the 出处 list; its
        // 1-based index is preserved for the ones that remain.
        let data = AnswerData {
            answer: "答案[1][2]".to_string(),
            citations: vec![
                AnswerCitation {
                    index: 1,
                    path: None,
                },
                AnswerCitation {
                    index: 2,
                    path: Some("/b.md".to_string()),
                },
            ],
        };
        let out = render_answer(&data);
        assert!(!out.contains("[1] `"));
        assert!(out.contains("[2] `/b.md`"));
    }

    #[test]
    fn render_answer_all_none_paths_body_only() {
        let data = AnswerData {
            answer: "答案".to_string(),
            citations: vec![AnswerCitation {
                index: 1,
                path: None,
            }],
        };
        assert_eq!(render_answer(&data), "答案");
    }
}
