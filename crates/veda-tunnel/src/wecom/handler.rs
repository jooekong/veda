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
use crate::qa_log::{QaLogEntry, QaLogStore};
use crate::registry::{self, Registry};
use crate::veda::{AnswerData, AnswerStreamItem, Hit, SearchError, VedaClient};

/// Minimum gap between interim WeCom frame refreshes while streaming.
/// Whether interim refreshes count against the 30 msg/min quota is not
/// documented — 1s throttling keeps a full answer under ~10 frames either way.
const STREAM_FLUSH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);
/// Stop interim refreshes when the accumulated text approaches WeCom's
/// 20480-byte frame cap; the final frame (rendered with sources) still goes out.
const STREAM_INTERIM_BYTE_CAP: usize = 19_000;

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
    /// QA telemetry sink (docs/plans/veda-tunnel-qa-log.md). Writes are
    /// best-effort; a failure warns and never blocks the reply.
    pub qa_log: Arc<QaLogStore>,
}

/// The server's canned refusal when retrieval found nothing relevant — MUST
/// stay in sync with veda-core `service::answer::NO_CONTEXT_ANSWER` (matched
/// as a prefix; the wire text may carry trailing punctuation). Rows matching
/// this are the "missing docs" backlog, the QA log's primary product.
const NO_CONTEXT_ANSWER: &str = "知识库中没有找到相关内容";

/// Citation entries shown on the compact "出处：" line; the rest collapse
/// into "等 N 篇". Grounded answers rarely cite more than this.
/// (Folding sources into a `<think>` block was probed 2026-07-16 and
/// abandoned: a final frame carrying think blocks stalled the bubble.)
const MAX_LISTED_CITATIONS: usize = 3;

/// A reply plus what the QA log needs to know about how it came to be.
struct Reply {
    text: String,
    outcome: &'static str,
    hit_count: u32,
    citation_count: u32,
}

pub async fn handle_message(ctx: HandlerCtx, req_id: String, body: MsgCallbackBody) {
    // One stream id per question; reused across the placeholder + final frame
    // so WeCom refreshes the same bubble.
    let stream_id = uuid::Uuid::new_v4().to_string();

    if body.msgtype.as_deref() != Some(MSGTYPE_TEXT) {
        // Guidance bubbles: no feedback UI, no QA-log row — not a Q&A.
        send_final(&ctx, &req_id, &stream_id, "暂只支持文字提问").await;
        return;
    }

    // Chat metadata for the QA log, captured before `text` is moved.
    let chat_type = body.chattype.clone().unwrap_or_else(|| "single".to_string());
    let user_id = body
        .from
        .as_ref()
        .and_then(|f| f.userid.clone())
        .unwrap_or_default();
    // Group chats key by chatid; singles by the asking user. Doubles as the
    // reachable-chat ledger for future proactive push (directions T6).
    let chat_key = match body.chatid.as_deref() {
        Some(cid) if chat_type == "group" => cid.to_string(),
        _ => user_id.clone(),
    };

    let raw_text = body.text.map(|t| t.content).unwrap_or_default();
    let query = strip_mention(&raw_text);
    if query.is_empty() {
        send_final(&ctx, &req_id, &stream_id, "请在 @ 机器人后输入要查询的问题").await;
        return;
    }

    // The feedback id rides the FIRST frame (protocol: stream.feedback.id on
    // the first reply activates WeCom's thumb-up/down UI); votes come back as
    // feedback_event callbacks carrying it.
    let feedback_id = uuid::Uuid::new_v4().to_string();

    // Placeholder frame — must reach WeCom within 5s; search may take longer.
    let _ = ctx
        .outbound
        .send(respond_stream_frame(
            &req_id,
            &stream_id,
            "正在查阅知识库…",
            false,
            Some(&feedback_id),
        ))
        .await;

    registry::update(&ctx.registry, &ctx.bot.bot_id, |s| {
        s.last_msg_at = Some(Utc::now());
        s.msg_count += 1;
    });

    let started = std::time::Instant::now();
    let reply = if ctx.answer_enabled {
        answer_reply_stream(&ctx, &req_id, &stream_id, &query, started).await
    } else {
        search_reply(&ctx, &query, started).await
    };

    send_final(&ctx, &req_id, &stream_id, &reply.text).await;

    let entry = QaLogEntry {
        bot_id: ctx.bot.bot_id.clone(),
        chat_type,
        chat_key,
        user_id,
        query,
        outcome: reply.outcome,
        hit_count: reply.hit_count,
        citation_count: reply.citation_count,
        latency_ms: started.elapsed().as_millis().min(u32::MAX as u128) as u32,
        answer_text: reply.text,
        feedback_id,
    };
    if let Err(e) = ctx.qa_log.log(&entry).await {
        warn!(bot = %ctx.bot.name, error = %e, "qa log write failed (reply already sent)");
    }
}

fn err_reply(text: &str, outcome: &'static str) -> Reply {
    Reply {
        text: text.to_string(),
        outcome,
        hit_count: 0,
        citation_count: 0,
    }
}

/// Pre-answer path: raw `/v1/search` + snippet rendering. Behaviour is
/// unchanged from before the answer switch existed.
async fn search_reply(ctx: &HandlerCtx, query: &str, started: std::time::Instant) -> Reply {
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
            Reply {
                text: render_markdown(&hits),
                outcome: if hits.is_empty() { "no_context" } else { "raw_search" },
                hit_count: hits.len() as u32,
                citation_count: 0,
            }
        }
        Err(SearchError::Unauthorized) => {
            registry::update(&ctx.registry, &ctx.bot.bot_id, |s| {
                s.error_count += 1;
                s.last_error = Some("search 401 (key invalid/revoked)".to_string());
            });
            // Keep the WeCom connection alive — a bad key is not a reason to
            // drop the long connection (§9).
            warn!(bot = %ctx.bot.name, "search unauthorized (401)");
            err_reply("知识库鉴权失败，请联系管理员", "error")
        }
        Err(SearchError::Unavailable(e)) => {
            registry::update(&ctx.registry, &ctx.bot.bot_id, |s| {
                s.error_count += 1;
                s.last_error = Some(truncate(&e, 200));
            });
            warn!(bot = %ctx.bot.name, error = %e, "search unavailable");
            err_reply("知识库暂时不可用，请稍后再试", "error")
        }
        // search() only maps 401→Unauthorized and everything else→Unavailable,
        // so these answer-only variants never reach here; treat them as
        // unavailable to keep the match exhaustive.
        Err(SearchError::Disabled)
        | Err(SearchError::Throttled)
        | Err(SearchError::StreamUnsupported) => {
            err_reply("知识库暂时不可用，请稍后再试", "error")
        }
    }
}

/// Success mapping shared by the one-shot and streaming answer paths.
/// Semantic retrieval always returns top-k (hit_count is never 0 in
/// practice — live-verified 2026-07-14), so "the KB doesn't cover this" is
/// signalled by the server's canned refusal text, not by hits. Hits with a
/// real answer but zero citations = ungrounded.
fn answer_data_to_reply(data: &AnswerData) -> Reply {
    let outcome = if data.answer.trim().starts_with(NO_CONTEXT_ANSWER) {
        "no_context"
    } else if data.citations.is_empty() {
        "ungrounded"
    } else {
        "answered"
    };
    Reply {
        text: render_answer(data),
        outcome,
        hit_count: data.hit_count as u32,
        citation_count: data.citations.len() as u32,
    }
}

/// Error mapping shared by the one-shot and streaming answer paths: bump the
/// bot's error counters and pick the user-facing canned phrase.
fn answer_error_to_reply(ctx: &HandlerCtx, e: SearchError) -> Reply {
    match e {
        SearchError::Disabled => {
            registry::update(&ctx.registry, &ctx.bot.bot_id, |s| {
                s.error_count += 1;
                s.last_error = Some("answer disabled (501)".to_string());
            });
            warn!(bot = %ctx.bot.name, "answer disabled (501)");
            err_reply("知识库问答未启用", "disabled")
        }
        SearchError::Throttled => {
            registry::update(&ctx.registry, &ctx.bot.bot_id, |s| {
                s.error_count += 1;
                s.last_error = Some("answer throttled (429)".to_string());
            });
            warn!(bot = %ctx.bot.name, "answer throttled (429)");
            err_reply("提问太频繁，请稍后再试", "throttled")
        }
        SearchError::Unauthorized => {
            registry::update(&ctx.registry, &ctx.bot.bot_id, |s| {
                s.error_count += 1;
                s.last_error = Some("answer 401 (key invalid/revoked)".to_string());
            });
            // Keep the WeCom connection alive — a bad key is not a reason to
            // drop the long connection (§9).
            warn!(bot = %ctx.bot.name, "answer unauthorized (401)");
            err_reply("知识库鉴权失败，请联系管理员", "error")
        }
        // StreamUnsupported is handled by the caller's fallback before this.
        SearchError::StreamUnsupported | SearchError::Unavailable(_) => {
            let detail = match e {
                SearchError::Unavailable(m) => m,
                _ => "stream unsupported".to_string(),
            };
            registry::update(&ctx.registry, &ctx.bot.bot_id, |s| {
                s.error_count += 1;
                s.last_error = Some(truncate(&detail, 200));
            });
            warn!(bot = %ctx.bot.name, error = %detail, "answer unavailable");
            err_reply("知识库暂时不可用，请稍后再试", "error")
        }
    }
}

/// Answer path: veda's `/v1/answer` RAG endpoint — body + a verifiable
/// citation list, with per-error fallbacks (§8). Used directly when the
/// server has no streaming endpoint yet, and as the streaming fallback.
async fn answer_reply(ctx: &HandlerCtx, query: &str, started: std::time::Instant) -> Reply {
    match ctx
        .veda
        .answer(&ctx.bot.veda_key, query, ctx.bot.prompt.as_deref())
        .await
    {
        Ok(data) => {
            let query_log: String = query.chars().take(64).collect();
            info!(
                bot = %ctx.bot.name,
                query = ?query_log,
                citations = data.citations.len(),
                hits = data.hit_count,
                ms = started.elapsed().as_millis() as u64,
                "answer ok"
            );
            answer_data_to_reply(&data)
        }
        Err(e) => answer_error_to_reply(ctx, e),
    }
}

/// Streaming answer: forwards LLM deltas to WeCom as throttled interim
/// refreshes of the same bubble (full-replacement semantics), then lets the
/// caller send the authoritative final frame from the returned `Reply`.
/// Falls back to the one-shot path when the server predates
/// `/v1/answer/stream`.
async fn answer_reply_stream(
    ctx: &HandlerCtx,
    req_id: &str,
    stream_id: &str,
    query: &str,
    started: std::time::Instant,
) -> Reply {
    let mut rx = match ctx
        .veda
        .answer_stream(&ctx.bot.veda_key, query, ctx.bot.prompt.as_deref())
        .await
    {
        Ok(rx) => rx,
        Err(SearchError::StreamUnsupported) => {
            info!(bot = %ctx.bot.name, "server has no /v1/answer/stream, falling back");
            return answer_reply(ctx, query, started).await;
        }
        Err(e) => return answer_error_to_reply(ctx, e),
    };

    let mut acc = String::new();
    let mut last_flush = std::time::Instant::now(); // placeholder frame just went out
    let mut frames_sent = 0u32;
    loop {
        match rx.recv().await {
            Some(AnswerStreamItem::Delta(d)) => {
                acc.push_str(&d);
                if last_flush.elapsed() >= STREAM_FLUSH_INTERVAL
                    && !acc.trim().is_empty()
                    && acc.len() <= STREAM_INTERIM_BYTE_CAP
                {
                    let _ = ctx
                        .outbound
                        .send(respond_stream_frame(req_id, stream_id, &acc, false, None))
                        .await;
                    frames_sent += 1;
                    last_flush = std::time::Instant::now();
                }
            }
            Some(AnswerStreamItem::Reset) => {
                // Server rolled back a talk-then-tool-call round: drop what
                // we accumulated. Any interim frame already shown gets
                // overwritten by the next flush / the final frame (WeCom
                // stream frames are full replacements).
                acc.clear();
            }
            Some(AnswerStreamItem::ToolNote { name, detail }) => {
                // Status line while tools run — the longest silent stretch of
                // an answer. Sent as a full-replacement frame; `acc` is empty
                // here (the server emits Reset before tool notes), and even if
                // it weren't, the next delta flush overwrites the bubble.
                // Shares the interim throttle so a note burst (up to 5 calls
                // per round) can't blow the frame budget.
                if last_flush.elapsed() >= STREAM_FLUSH_INTERVAL {
                    let note = render_tool_note(&name, &detail);
                    let _ = ctx
                        .outbound
                        .send(respond_stream_frame(req_id, stream_id, &note, false, None))
                        .await;
                    frames_sent += 1;
                    last_flush = std::time::Instant::now();
                }
            }
            Some(AnswerStreamItem::Final(data)) => {
                let query_log: String = query.chars().take(64).collect();
                info!(
                    bot = %ctx.bot.name,
                    query = ?query_log,
                    citations = data.citations.len(),
                    hits = data.hit_count,
                    interim_frames = frames_sent,
                    ms = started.elapsed().as_millis() as u64,
                    "answer ok (streamed)"
                );
                return answer_data_to_reply(&data);
            }
            Some(AnswerStreamItem::Error(code)) => {
                let e = match code.as_str() {
                    "THROTTLED" => SearchError::Throttled,
                    "FEATURE_DISABLED" => SearchError::Disabled,
                    other => SearchError::Unavailable(format!("stream error: {other}")),
                };
                return answer_error_to_reply(ctx, e);
            }
            None => {
                return answer_error_to_reply(
                    ctx,
                    SearchError::Unavailable("stream ended without final".to_string()),
                );
            }
        }
    }
}

async fn send_final(ctx: &HandlerCtx, req_id: &str, stream_id: &str, content: &str) {
    // feedback.id only matters on the first frame of a reply; final frames
    // never (re)set it. Cap the body so an unusually long answer can't blow
    // past WeCom's ~20480-byte frame limit.
    let content = cap_frame_bytes(content, STREAM_INTERIM_BYTE_CAP);
    let _ = ctx
        .outbound
        .send(respond_stream_frame(req_id, stream_id, &content, true, None))
        .await;
}

/// Status line for a server-side tool call. Only the two known tools get a
/// specific verb; unknown tools (server newer than tunnel) and empty details
/// degrade to a generic note.
fn render_tool_note(name: &str, detail: &str) -> String {
    let detail = detail.trim();
    match (name, detail.is_empty()) {
        ("search", false) => format!("🔍 正在检索:{detail}"),
        ("read_file", false) => format!("📄 正在查阅:{detail}"),
        _ => "🔍 正在查阅知识库…".to_string(),
    }
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

/// Render a `/v1/answer` result for WeCom: the answer body, then one compact
/// "出处：" line of `[n]` + file basename entries. `[n]` reuses the
/// server-assigned citation index so it lines up with the `[n]` markers in
/// the body. At most [`MAX_LISTED_CITATIONS`] entries are shown; the rest
/// collapse into "等 N 篇". Citations without a resolvable path are skipped;
/// if none remain, only the body is sent.
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
    // Basenames keep the line short, but knowledge bases hold same-named
    // files in different dirs — entries whose basename collides within the
    // displayed set fall back to their full path.
    let shown = &cited[..cited.len().min(MAX_LISTED_CITATIONS)];
    let listed: Vec<String> = shown
        .iter()
        .map(|(idx, path)| {
            let name = basename(path);
            let dup = shown.iter().filter(|(_, p)| basename(p) == name).count() > 1;
            format!("[{idx}] `{}`", if dup { *path } else { name })
        })
        .collect();
    let mut out = format!("{body}\n\n出处：{}", listed.join(" · "));
    if cited.len() > MAX_LISTED_CITATIONS {
        out.push_str(&format!(" 等 {} 篇", cited.len()));
    }
    out
}

/// Last path segment for the compact source line ("/hr/报销.md" → "报销.md").
/// Falls back to the whole path when there is no non-empty segment.
fn basename(path: &str) -> &str {
    path.rsplit('/').find(|s| !s.is_empty()).unwrap_or(path)
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

/// Cap a final frame to `cap` bytes. WeCom rejects frames over 20480 bytes;
/// an unusually long answer body can exceed that. Under the cap the text is
/// returned untouched; over it, cut on a char boundary and append a
/// truncation marker (result stays ≤ cap bytes).
fn cap_frame_bytes(s: &str, cap: usize) -> String {
    if s.len() <= cap {
        return s.to_string();
    }
    const MARK: &str = "\n…(内容过长已截断)";
    let budget = cap.saturating_sub(MARK.len());
    let mut end = 0;
    for (i, c) in s.char_indices() {
        let next = i + c.len_utf8();
        if next > budget {
            break;
        }
        end = next;
    }
    format!("{}{MARK}", &s[..end])
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
    fn tool_note_renders_by_tool() {
        assert_eq!(render_tool_note("search", "DAL 多活"), "🔍 正在检索:DAL 多活");
        assert_eq!(render_tool_note("read_file", "/a/b.md"), "📄 正在查阅:/a/b.md");
        // Unknown tool / blank detail → generic note.
        assert_eq!(render_tool_note("mystery", "x"), "🔍 正在查阅知识库…");
        assert_eq!(render_tool_note("search", "  "), "🔍 正在查阅知识库…");
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
    fn cap_frame_leaves_text_within_cap() {
        let s = "a".repeat(19_000); // exactly at cap → untouched
        assert_eq!(cap_frame_bytes(&s, 19_000), s);
    }

    #[test]
    fn cap_frame_truncates_oversize_on_char_boundary() {
        let s = "中".repeat(10_000); // 30_000 bytes, well over cap
        let out = cap_frame_bytes(&s, 19_000);
        assert!(out.len() <= 19_000, "capped to <= cap bytes, got {}", out.len());
        let body = out.strip_suffix("\n…(内容过长已截断)").expect("has truncation marker");
        assert!(body.chars().all(|c| c == '中'), "body cut on a char boundary");
        assert!(!body.is_empty());
    }

    #[test]
    fn render_answer_with_citations() {
        let data = AnswerData {
            hit_count: 5,
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
        // Compact single source line: basenames joined on one line.
        assert!(out.contains("出处：[1] `接入.md` · [2] `多活.md`"), "{out}");
        assert!(!out.contains("/a/接入.md"), "full paths are not shown");
        assert_eq!(out.lines().filter(|l| l.contains("出处：")).count(), 1);
    }

    #[test]
    fn render_answer_folds_beyond_max_listed() {
        let citations = (1..=5)
            .map(|i| AnswerCitation {
                index: i,
                path: Some(format!("/docs/f{i}.md")),
            })
            .collect();
        let data = AnswerData {
            hit_count: 5,
            answer: "答案[1][2][3][4][5]".to_string(),
            citations,
        };
        let out = render_answer(&data);
        assert!(out.contains("[3] `f3.md`"), "{out}");
        assert!(!out.contains("f4.md"), "entries beyond the cap are folded");
        assert!(out.contains("等 5 篇"), "{out}");
    }

    #[test]
    fn render_answer_disambiguates_duplicate_basenames() {
        let data = AnswerData {
            hit_count: 5,
            answer: "答案[1][2][3]".to_string(),
            citations: vec![
                AnswerCitation {
                    index: 1,
                    path: Some("/dal/接入.md".to_string()),
                },
                AnswerCitation {
                    index: 2,
                    path: Some("/fdc/接入.md".to_string()),
                },
                AnswerCitation {
                    index: 3,
                    path: Some("/dal/faq.md".to_string()),
                },
            ],
        };
        let out = render_answer(&data);
        // Colliding basenames show their full path; unique ones stay short.
        assert!(out.contains("[1] `/dal/接入.md`"), "{out}");
        assert!(out.contains("[2] `/fdc/接入.md`"), "{out}");
        assert!(out.contains("[3] `faq.md`"), "{out}");
    }

    #[test]
    fn basename_extracts_last_segment() {
        assert_eq!(basename("/hr/报销.md"), "报销.md");
        assert_eq!(basename("plain.md"), "plain.md");
        assert_eq!(basename("/dir/"), "dir");
        assert_eq!(basename("/"), "/");
    }

    #[test]
    fn render_answer_no_citations_body_only() {
        let data = AnswerData {
            hit_count: 5,
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
            hit_count: 5,
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
        assert!(out.contains("[2] `b.md`"));
    }

    #[test]
    fn render_answer_all_none_paths_body_only() {
        let data = AnswerData {
            hit_count: 5,
            answer: "答案".to_string(),
            citations: vec![AnswerCitation {
                index: 1,
                path: None,
            }],
        };
        assert_eq!(render_answer(&data), "答案");
    }

    #[test]
    fn answer_outcome_classification() {
        let data = |answer: &str, citations: Vec<AnswerCitation>| AnswerData {
            hit_count: 5,
            answer: answer.to_string(),
            citations,
        };
        // Canned refusal → no_context, regardless of citations.
        let r = answer_data_to_reply(&data("知识库中没有找到相关内容。", vec![]));
        assert_eq!(r.outcome, "no_context");
        // Real answer with zero citations → ungrounded (the server no longer
        // backfills all evidence blocks, so this branch is reachable).
        let r = answer_data_to_reply(&data("未标注的回答", vec![]));
        assert_eq!(r.outcome, "ungrounded");
        // Cited answer → answered.
        let r = answer_data_to_reply(&data(
            "答案[1]",
            vec![AnswerCitation {
                index: 1,
                path: Some("/a.md".to_string()),
            }],
        ));
        assert_eq!(r.outcome, "answered");
        assert_eq!(r.citation_count, 1);
    }
}
