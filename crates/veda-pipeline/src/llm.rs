use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tracing::warn;
use veda_core::store::{ChatMsg, ChatStreamItem, LlmService, ToolCall, ToolSpec};
use veda_types::{Result, VedaError};

const MAX_RETRIES: u32 = 3;
const BASE_BACKOFF_MS: u64 = 500;

/// Wire mirror of one chat message (OpenAI chat/completions shape).
#[derive(Debug, Serialize)]
struct ChatMessage {
    role: String,
    content: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<ToolCallWire>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

impl ChatMessage {
    fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".to_string(),
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }
}

impl From<&ChatMsg> for ChatMessage {
    fn from(m: &ChatMsg) -> Self {
        Self {
            role: m.role.clone(),
            content: m.content.clone(),
            tool_calls: m.tool_calls.iter().map(ToolCallWire::from).collect(),
            tool_call_id: m.tool_call_id.clone(),
        }
    }
}

/// `{"id","type":"function","function":{"name","arguments"}}` — the shape
/// tool calls take both in assistant echo messages and in responses.
#[derive(Debug, Serialize)]
struct ToolCallWire {
    id: String,
    r#type: &'static str,
    function: FunctionWire,
}

#[derive(Debug, Serialize)]
struct FunctionWire {
    name: String,
    arguments: String,
}

impl From<&ToolCall> for ToolCallWire {
    fn from(c: &ToolCall) -> Self {
        Self {
            id: c.id.clone(),
            r#type: "function",
            function: FunctionWire {
                name: c.name.clone(),
                arguments: c.arguments.clone(),
            },
        }
    }
}

/// `{"type":"function","function":{name,description,parameters}}`.
#[derive(Debug, Serialize)]
struct ToolSpecWire {
    r#type: &'static str,
    function: ToolFnWire,
}

#[derive(Debug, Serialize)]
struct ToolFnWire {
    name: &'static str,
    description: &'static str,
    parameters: serde_json::Value,
}

impl From<&ToolSpec> for ToolSpecWire {
    fn from(t: &ToolSpec) -> Self {
        Self {
            r#type: "function",
            function: ToolFnWire {
                name: t.name,
                description: t.description,
                parameters: t.parameters.clone(),
            },
        }
    }
}

#[derive(Debug, Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<ToolSpecWire>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<usize>,
    temperature: f32,
    /// OpenAI-compatible streaming switch; false is the wire default but we
    /// always send it explicitly to keep both code paths symmetrical.
    stream: bool,
    /// Non-standard gateway switch (company airouter): turns a reasoning
    /// model's thinking off for this call. MUST stay absent unless a
    /// deployment opts in — the OpenAI API itself rejects unknown top-level
    /// params with a 400, so an unconditional field would break every
    /// standards-compliant backend.
    #[serde(skip_serializing_if = "Option::is_none")]
    enable_thinking: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    message: ChatMessageResp,
}

#[derive(Debug, Deserialize)]
struct ChatMessageResp {
    content: String,
}

pub struct LlmProvider {
    client: reqwest::Client,
    api_url: String,
    api_key: String,
    model: String,
    /// Send `enable_thinking: false` on the summarize path. Off unless the
    /// deployment's gateway is known to accept it (see `ChatRequest`).
    summary_disable_thinking: bool,
}

impl LlmProvider {
    pub fn new(
        api_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
        summary_disable_thinking: bool,
    ) -> Result<Self> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|e| VedaError::Internal(e.to_string()))?;
        Ok(Self {
            client,
            api_url: api_url.into(),
            api_key: api_key.into(),
            model: model.into(),
            summary_disable_thinking,
        })
    }

    /// Body for the non-streaming summarize path. Split out from
    /// `chat_once` so the wire shape — above all whether `enable_thinking`
    /// is present — is unit testable without a network.
    fn summary_request(&self, prompt: &str, max_tokens: usize) -> ChatRequest {
        ChatRequest {
            model: self.model.clone(),
            messages: vec![ChatMessage::user(prompt)],
            tools: Vec::new(),
            max_tokens: Some(max_tokens),
            temperature: 0.0,
            stream: false,
            // Summaries want text, not reasoning: on airouter the thinking
            // ate ~87% of the completion tokens and shared the max_tokens
            // budget with the answer. `false` only when configured; `None`
            // keeps the field off the wire entirely.
            enable_thinking: self.summary_disable_thinking.then_some(false),
        }
    }

    /// Body for the streaming `/v1/answer` path. `enable_thinking` is
    /// deliberately never set here: answers benefit from the model
    /// reasoning, and this path is not the one that starved on tokens.
    fn stream_request(
        &self,
        messages: &[ChatMsg],
        tools: &[ToolSpec],
        max_tokens: usize,
    ) -> ChatRequest {
        ChatRequest {
            model: self.model.clone(),
            messages: messages.iter().map(ChatMessage::from).collect(),
            tools: tools.iter().map(ToolSpecWire::from).collect(),
            max_tokens: Some(max_tokens),
            temperature: 0.0,
            stream: true,
            enable_thinking: None,
        }
    }

    async fn chat_once(&self, prompt: &str, max_tokens: usize) -> std::result::Result<String, LlmError> {
        let body = self.summary_request(prompt, max_tokens);

        let mut req = self.client.post(&self.api_url).json(&body);
        if !self.api_key.is_empty() {
            req = req.bearer_auth(&self.api_key);
        }

        let response = req.send().await.map_err(|e| LlmError {
            inner: VedaError::Internal(format!("LLM request failed: {e}")),
            retryable: true,
        })?;

        let status = response.status();
        let bytes = response.bytes().await.map_err(|e| LlmError {
            inner: VedaError::Internal(format!("LLM read body failed: {e}")),
            retryable: true,
        })?;

        let retryable = status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error();
        if !status.is_success() {
            let msg = String::from_utf8_lossy(&bytes).into_owned();
            return Err(LlmError {
                inner: VedaError::Internal(format!("LLM HTTP {status}: {msg}")),
                retryable,
            });
        }

        parse_chat_response(&bytes)
    }

    async fn chat(&self, prompt: &str, max_tokens: usize) -> Result<String> {
        let mut last_err = None;
        for attempt in 0..=MAX_RETRIES {
            match self.chat_once(prompt, max_tokens).await {
                Ok(v) => return Ok(v),
                Err(e) => {
                    if !e.retryable || attempt == MAX_RETRIES {
                        return Err(e.inner);
                    }
                    let backoff_ms = BASE_BACKOFF_MS * 2u64.pow(attempt);
                    warn!(attempt, backoff_ms, err = %e.inner, "LLM call failed, retrying");
                    tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                    last_err = Some(e.inner);
                }
            }
        }
        Err(last_err.unwrap())
    }
}

#[derive(Debug)]
struct LlmError {
    inner: VedaError,
    retryable: bool,
}

/// Parse a non-streaming chat completion body into the message content.
///
/// Empty content (after trim) is an error, and a retryable one: summarize()
/// prompts always demand text, so an empty completion means upstream
/// misbehavior — e.g. a reasoning model whose thinking exhausted max_tokens
/// (the 2026-07 empty-abstract incident, where HTTP 200 + content="" was
/// silently persisted). Retrying may land on a healthy backend; if all
/// retries return empty the task fails loudly instead of storing "".
fn parse_chat_response(bytes: &[u8]) -> std::result::Result<String, LlmError> {
    let parsed: ChatResponse = serde_json::from_slice(bytes).map_err(|e| LlmError {
        inner: VedaError::Internal(format!("LLM invalid JSON: {e}")),
        retryable: false,
    })?;

    let content = parsed
        .choices
        .into_iter()
        .next()
        .map(|c| c.message.content.trim().to_string())
        .ok_or_else(|| LlmError {
            inner: VedaError::Internal("LLM returned empty choices".to_string()),
            retryable: false,
        })?;

    if content.is_empty() {
        return Err(LlmError {
            inner: VedaError::Internal("LLM returned empty content".to_string()),
            retryable: true,
        });
    }
    Ok(content)
}

// ── OpenAI-compatible SSE stream parsing ────────────────
// Kept as pure incremental pieces so byte-chunk boundary handling is unit
// testable without a network. tunnel/veda.rs carries a sibling copy of this
// parser for the server's own SSE surface — keep behavioural fixes in sync.

/// Splits an SSE byte stream into complete lines across chunk boundaries.
#[derive(Default)]
struct SseLineBuffer {
    buf: Vec<u8>,
}

impl SseLineBuffer {
    /// Feed one byte chunk; returns every *complete* line (newline-stripped).
    /// Bytes accumulate raw and are decoded only once a line is complete, so a
    /// multi-byte char split across chunks is reassembled before the lossy
    /// decode — a mid-char boundary never turns into U+FFFD.
    fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line);
            out.push(line.trim_end_matches(['\n', '\r']).to_string());
        }
        out
    }
}

/// One parsed SSE line from an OpenAI-compatible chat stream.
#[derive(Debug, PartialEq)]
enum SseLine {
    /// A delta frame: optional content plus tool-call fragments (either may
    /// be empty — e.g. the role-only first frame).
    Delta(StreamDelta),
    /// `data: [DONE]` — generation finished.
    Done,
    /// Comment / blank / non-data / unparsable line — ignore.
    Skip,
}

#[derive(Debug, Deserialize)]
struct StreamChunk {
    #[serde(default)]
    choices: Vec<StreamChoice>,
}
#[derive(Debug, Deserialize)]
struct StreamChoice {
    #[serde(default)]
    delta: StreamDelta,
}
#[derive(Debug, Deserialize, Default, PartialEq)]
struct StreamDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<StreamToolCallFrag>,
}

/// One streamed tool-call fragment. First fragment of a call carries
/// `id` + `function.name`; subsequent ones append to `function.arguments`.
/// Fragments are keyed by `index` (parallel calls interleave).
#[derive(Debug, Deserialize, PartialEq)]
struct StreamToolCallFrag {
    #[serde(default)]
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<FragFn>,
}

#[derive(Debug, Deserialize, PartialEq)]
struct FragFn {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

fn parse_sse_line(line: &str) -> SseLine {
    let Some(data) = line.strip_prefix("data:") else {
        return SseLine::Skip;
    };
    let data = data.trim();
    if data == "[DONE]" {
        return SseLine::Done;
    }
    match serde_json::from_str::<StreamChunk>(data) {
        Ok(c) => SseLine::Delta(
            c.choices
                .into_iter()
                .next()
                .map(|ch| ch.delta)
                .unwrap_or_default(),
        ),
        // A malformed frame shouldn't kill the stream — skip it.
        Err(_) => SseLine::Skip,
    }
}

/// Accumulates streamed tool-call fragments into complete calls, keyed by
/// `index`. Pure state machine — unit-tested against interleaved fragments.
#[derive(Default)]
struct ToolCallAssembler {
    calls: Vec<ToolCall>,
}

impl ToolCallAssembler {
    fn feed(&mut self, frag: StreamToolCallFrag) {
        // Defensive cap: rounds only execute 5 calls; a corrupt/hostile frame
        // must not force a huge allocation.
        if frag.index >= 16 {
            return;
        }
        while self.calls.len() <= frag.index {
            self.calls.push(ToolCall {
                id: String::new(),
                name: String::new(),
                arguments: String::new(),
            });
        }
        let call = &mut self.calls[frag.index];
        if let Some(id) = frag.id {
            call.id.push_str(&id);
        }
        if let Some(f) = frag.function {
            if let Some(name) = f.name {
                call.name.push_str(&name);
            }
            if let Some(args) = f.arguments {
                call.arguments.push_str(&args);
            }
        }
    }

    fn is_empty(&self) -> bool {
        self.calls.is_empty()
    }

    /// Completed calls; nameless residue (never got a first fragment) is
    /// dropped rather than surfaced as an unusable call.
    fn finish(self) -> Vec<ToolCall> {
        self.calls.into_iter().filter(|c| !c.name.is_empty()).collect()
    }
}

#[async_trait]
impl LlmService for LlmProvider {
    async fn summarize(&self, content: &str, max_tokens: usize) -> Result<String> {
        let started = std::time::Instant::now();
        let result = self.chat(content, max_tokens).await;
        let outcome = if result.is_ok() { "ok" } else { "err" };
        ::metrics::histogram!(
            "veda_llm_latency_seconds",
            "outcome" => outcome,
        )
        .record(started.elapsed().as_secs_f64());
        ::metrics::counter!(
            "veda_llm_total",
            "outcome" => outcome,
        )
        .increment(1);
        result
    }

    // No retry here: the caller retries the whole call while nothing has
    // streamed yet; once items flow a break is surfaced as an Err item.
    async fn chat_stream(
        &self,
        messages: &[ChatMsg],
        tools: &[ToolSpec],
        max_tokens: usize,
    ) -> Result<tokio::sync::mpsc::Receiver<Result<ChatStreamItem>>> {
        use futures_util::StreamExt;

        let body = self.stream_request(messages, tools, max_tokens);
        let mut req = self.client.post(&self.api_url).json(&body);
        if !self.api_key.is_empty() {
            req = req.bearer_auth(&self.api_key);
        }
        let response = req
            .send()
            .await
            .map_err(|e| VedaError::Internal(format!("LLM stream request failed: {e}")))?;
        let status = response.status();
        if !status.is_success() {
            let msg = response.text().await.unwrap_or_default();
            return Err(VedaError::Internal(format!("LLM stream HTTP {status}: {msg}")));
        }

        let (tx, rx) = tokio::sync::mpsc::channel::<Result<ChatStreamItem>>(32);
        let mut bytes = response.bytes_stream();
        tokio::spawn(async move {
            let started = std::time::Instant::now();
            let mut buf = SseLineBuffer::default();
            let mut assembler = ToolCallAssembler::default();
            // Emits the assembled ToolCalls (if any) as the final item.
            // Shared by the [DONE] and EOF exits.
            async fn flush(
                assembler: ToolCallAssembler,
                tx: &tokio::sync::mpsc::Sender<Result<ChatStreamItem>>,
            ) {
                if !assembler.is_empty() {
                    let _ = tx.send(Ok(ChatStreamItem::ToolCalls(assembler.finish()))).await;
                }
            }
            while let Some(chunk) = bytes.next().await {
                match chunk {
                    Ok(b) => {
                        for line in buf.push(&b) {
                            match parse_sse_line(&line) {
                                SseLine::Delta(d) => {
                                    for frag in d.tool_calls {
                                        assembler.feed(frag);
                                    }
                                    if let Some(t) = d.content {
                                        if !t.is_empty()
                                            && tx
                                                .send(Ok(ChatStreamItem::Content(t)))
                                                .await
                                                .is_err()
                                        {
                                            return; // receiver dropped → cancel
                                        }
                                    }
                                }
                                SseLine::Done => {
                                    flush(assembler, &tx).await;
                                    record_llm_metrics("ok", started);
                                    return;
                                }
                                SseLine::Skip => {}
                            }
                        }
                    }
                    Err(e) => {
                        record_llm_metrics("err", started);
                        let _ = tx
                            .send(Err(VedaError::Internal(format!("LLM stream broke: {e}"))))
                            .await;
                        return;
                    }
                }
            }
            // EOF without [DONE]: some gateways just close — treat as done.
            flush(assembler, &tx).await;
            record_llm_metrics("ok", started);
        });
        Ok(rx)
    }
}

fn record_llm_metrics(outcome: &'static str, started: std::time::Instant) {
    ::metrics::histogram!("veda_llm_latency_seconds", "outcome" => outcome)
        .record(started.elapsed().as_secs_f64());
    ::metrics::counter!("veda_llm_total", "outcome" => outcome).increment(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Content-only delta frame, as most generation frames look.
    fn content_delta(s: &str) -> SseLine {
        SseLine::Delta(StreamDelta {
            content: Some(s.to_string()),
            tool_calls: Vec::new(),
        })
    }

    // ── request shape ────────────────────────────────────

    fn provider(summary_disable_thinking: bool) -> LlmProvider {
        // No request is ever sent — only the serialized body is inspected.
        LlmProvider::new("http://127.0.0.1:1/v1/chat/completions", "", "m", summary_disable_thinking)
            .expect("build provider")
    }

    fn body(req: &ChatRequest) -> serde_json::Value {
        serde_json::to_value(req).expect("serialize request")
    }

    #[test]
    fn summary_request_omits_enable_thinking_by_default() {
        // The default must stay wire-compatible with the OpenAI API, which
        // 400s on unknown top-level params: the key is absent, not `null`.
        let v = body(&provider(false).summary_request("hi", 8192));
        assert!(
            v.get("enable_thinking").is_none(),
            "enable_thinking must not be sent unless configured: {v}"
        );
        assert_eq!(v["stream"], serde_json::json!(false));
    }

    #[test]
    fn summary_request_disables_thinking_when_configured() {
        let v = body(&provider(true).summary_request("hi", 8192));
        assert_eq!(v["enable_thinking"], serde_json::json!(false));
    }

    #[test]
    fn stream_request_never_disables_thinking() {
        // Answers keep their reasoning under every configuration — the
        // switch is scoped to summaries on purpose.
        for disable in [false, true] {
            let v = body(&provider(disable).stream_request(&[ChatMsg::user("q")], &[], 4096));
            assert!(
                v.get("enable_thinking").is_none(),
                "stream path must never send enable_thinking (disable={disable}): {v}"
            );
            assert_eq!(v["stream"], serde_json::json!(true));
        }
    }

    // ── parse_chat_response ──────────────────────────────

    #[test]
    fn parse_response_returns_trimmed_content() {
        let body = br#"{"choices":[{"message":{"content":"  a summary  "}}]}"#;
        assert_eq!(parse_chat_response(body).unwrap(), "a summary");
    }

    #[test]
    fn parse_response_empty_content_is_retryable_error() {
        // The 2026-07 incident shape: HTTP 200, choices present, content "".
        let body = br#"{"choices":[{"message":{"content":""}}]}"#;
        let err = parse_chat_response(body).unwrap_err();
        assert!(err.retryable, "empty content must be retryable");
        assert!(err.inner.to_string().contains("empty content"));

        // Whitespace-only content is the same failure after trim.
        let body = br#"{"choices":[{"message":{"content":"  \n "}}]}"#;
        assert!(parse_chat_response(body).unwrap_err().retryable);
    }

    #[test]
    fn parse_response_empty_choices_is_terminal_error() {
        let body = br#"{"choices":[]}"#;
        let err = parse_chat_response(body).unwrap_err();
        assert!(!err.retryable);
    }

    #[test]
    fn parse_response_invalid_json_is_terminal_error() {
        let err = parse_chat_response(b"{broken").unwrap_err();
        assert!(!err.retryable);
    }

    #[test]
    fn sse_buffer_reassembles_split_lines() {
        let mut b = SseLineBuffer::default();
        assert!(b.push(b"data: {\"choices\":[{\"del").is_empty());
        let lines = b.push(b"ta\":{\"content\":\"hi\"}}]}\n\ndata: [DO");
        assert_eq!(lines.len(), 2); // the data line + the blank separator
        assert_eq!(parse_sse_line(&lines[0]), content_delta("hi"));
        assert_eq!(parse_sse_line(&lines[1]), SseLine::Skip);
        let lines = b.push(b"NE]\n");
        assert_eq!(parse_sse_line(&lines[0]), SseLine::Done);
    }

    #[test]
    fn sse_buffer_reassembles_utf8_split_across_chunks() {
        // A CJK char (3 bytes) split across two network chunks must survive:
        // the buffer decodes only complete lines, so no byte lands mid-char.
        let line = "data: {\"choices\":[{\"delta\":{\"content\":\"中文答案\"}}]}\n";
        let bytes = line.as_bytes();
        let cut = line.find('中').unwrap() + 1; // 1 byte into the first CJK char
        let mut b = SseLineBuffer::default();
        assert!(b.push(&bytes[..cut]).is_empty());
        let lines = b.push(&bytes[cut..]);
        assert_eq!(lines.len(), 1);
        assert!(!lines[0].contains('\u{FFFD}'), "no replacement char: {}", lines[0]);
        assert_eq!(parse_sse_line(&lines[0]), content_delta("中文答案"));
    }

    #[test]
    fn parse_skips_noise_and_role_frames() {
        assert_eq!(parse_sse_line(": keepalive"), SseLine::Skip);
        assert_eq!(parse_sse_line(""), SseLine::Skip);
        assert_eq!(parse_sse_line("event: message"), SseLine::Skip);
        // role-only first frame → empty delta (sender drops empties)
        assert_eq!(
            parse_sse_line(r#"data: {"choices":[{"delta":{"role":"assistant"}}]}"#),
            SseLine::Delta(StreamDelta::default())
        );
        // malformed json must not kill the stream
        assert_eq!(parse_sse_line("data: {broken"), SseLine::Skip);
    }

    #[test]
    fn parse_crlf_and_spacing_variants() {
        let mut b = SseLineBuffer::default();
        let lines = b.push(b"data:{\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\r\n");
        assert_eq!(parse_sse_line(&lines[0]), content_delta("a"));
        assert_eq!(parse_sse_line("data:  [DONE]"), SseLine::Done);
    }

    #[test]
    fn parse_tool_call_fragment_line() {
        let line = r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"search","arguments":""}}]}}]}"#;
        match parse_sse_line(line) {
            SseLine::Delta(d) => {
                assert_eq!(d.content, None);
                assert_eq!(d.tool_calls.len(), 1);
                let f = &d.tool_calls[0];
                assert_eq!(f.index, 0);
                assert_eq!(f.id.as_deref(), Some("call_1"));
                assert_eq!(f.function.as_ref().unwrap().name.as_deref(), Some("search"));
            }
            other => panic!("expected delta, got {other:?}"),
        }
    }

    // ── ToolCallAssembler ──────────────────────────────

    fn frag(index: usize, id: Option<&str>, name: Option<&str>, args: Option<&str>) -> StreamToolCallFrag {
        StreamToolCallFrag {
            index,
            id: id.map(String::from),
            function: (name.is_some() || args.is_some()).then(|| FragFn {
                name: name.map(String::from),
                arguments: args.map(String::from),
            }),
        }
    }

    #[test]
    fn assembler_joins_argument_fragments() {
        let mut a = ToolCallAssembler::default();
        a.feed(frag(0, Some("call_1"), Some("search"), None));
        a.feed(frag(0, None, None, Some("{\"query\":")));
        a.feed(frag(0, None, None, Some("\"DAL 接入\"}")));
        let calls = a.finish();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].name, "search");
        assert_eq!(calls[0].arguments, r#"{"query":"DAL 接入"}"#);
    }

    #[test]
    fn assembler_handles_interleaved_parallel_calls() {
        let mut a = ToolCallAssembler::default();
        a.feed(frag(0, Some("c0"), Some("search"), Some("{\"q")));
        a.feed(frag(1, Some("c1"), Some("read_file"), Some("{\"p")));
        a.feed(frag(0, None, None, Some("\":1}")));
        a.feed(frag(1, None, None, Some("\":2}")));
        let calls = a.finish();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "search");
        assert_eq!(calls[0].arguments, "{\"q\":1}");
        assert_eq!(calls[1].name, "read_file");
        assert_eq!(calls[1].arguments, "{\"p\":2}");
    }

    #[test]
    fn assembler_drops_nameless_residue() {
        let mut a = ToolCallAssembler::default();
        // index 1 arrives before index 0 ever gets a first fragment
        a.feed(frag(1, Some("c1"), Some("search"), Some("{}")));
        let calls = a.finish();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "search");
    }

    #[test]
    fn assembler_caps_absurd_index() {
        // A hostile frame with a huge index must be dropped, not allocate.
        let mut a = ToolCallAssembler::default();
        a.feed(frag(10_000, Some("c"), Some("search"), Some("{}")));
        assert!(a.finish().is_empty());
    }

    /// Live smoke against a real OpenAI-compatible gateway — the earliest
    /// hard evidence for the streamed tool_calls fragment shape this module
    /// assumes (the design's biggest external assumption). Run manually:
    ///   VEDA_LLM_API_URL=… VEDA_LLM_API_KEY=… VEDA_LLM_MODEL=… \
    ///     cargo test -p veda-pipeline --lib -- --ignored tool_calls_smoke
    #[tokio::test]
    #[ignore]
    async fn tool_calls_smoke() {
        let url = std::env::var("VEDA_LLM_API_URL").expect("set VEDA_LLM_API_URL");
        let key = std::env::var("VEDA_LLM_API_KEY").unwrap_or_default();
        let model = std::env::var("VEDA_LLM_MODEL").expect("set VEDA_LLM_MODEL");
        let provider = LlmProvider::new(url, key, model, false).unwrap();

        let tools = [ToolSpec {
            name: "search",
            description: "在知识库中检索资料",
            parameters: serde_json::json!({
                "type": "object",
                "properties": {"query": {"type": "string", "description": "检索词"}},
                "required": ["query"]
            }),
        }];
        let messages = [
            ChatMsg::system("你必须调用 search 工具查资料后再回答,不要直接作答。"),
            ChatMsg::user("如何接入 DAL?"),
        ];
        let mut rx = provider.chat_stream(&messages, &tools, 256).await.expect("connect");
        let mut contents = String::new();
        let mut tool_calls = None;
        while let Some(item) = rx.recv().await {
            match item.expect("stream item") {
                ChatStreamItem::Content(t) => contents.push_str(&t),
                ChatStreamItem::ToolCalls(c) => tool_calls = Some(c),
            }
        }
        let calls = tool_calls.expect("model should have called the tool");
        eprintln!("content: {contents:?}\ntool_calls: {calls:?}");
        assert_eq!(calls[0].name, "search");
        assert!(
            serde_json::from_str::<serde_json::Value>(&calls[0].arguments).is_ok(),
            "arguments should be valid JSON: {}",
            calls[0].arguments
        );
    }
}
