use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tracing::warn;
use veda_core::store::{ChatMsg, ChatStreamItem, LlmService, ToolCall, ToolSpec};
use veda_types::{Result, VedaError};

const MAX_RETRIES: u32 = 3;
const BASE_BACKOFF_MS: u64 = 500;
/// First quota (HTTP 429) backoff, tripling per attempt: 1s / 3s / 9s.
///
/// Deliberately seconds and not minutes. Probing the company airouter under
/// real 429s (2026-08-04, 73 calls) showed the limiter is *instantaneous
/// concurrency*, not a minute-long lockout: 429s and 200s interleave inside
/// the same second, a rejection comes back in 0.13s from the gateway itself,
/// and throughput recovers the moment pressure drops. Minute-scale sleeps
/// would idle the worker through a window that has already reopened. The
/// responses carry no `Retry-After` and no rate-limit headers (the backend is
/// Aliyun Bailian), so there is nothing to obey — the schedule is ours.
const QUOTA_BACKOFF_BASE_MS: u64 = 1000;
const QUOTA_BACKOFF_FACTOR: u64 = 3;

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
    max_tokens: usize,
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
    /// Why generation stopped — "stop", "length", … Only read when the
    /// content came back empty, where it separates "hit the token ceiling"
    /// from "upstream returned nothing for another reason". Some gateways
    /// omit it, hence Option.
    #[serde(default)]
    finish_reason: Option<String>,
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
    /// Second model to try once the primary has burned its retries on quota
    /// 429s. `None` (the default) keeps the old behaviour exactly.
    summary_fallback_model: Option<String>,
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
            summary_fallback_model: None,
        })
    }

    /// Opt into summary fallback (see `chat`). A builder step rather than a
    /// `new` parameter so the four existing call sites stay untouched — and
    /// so "not configured" remains the shape you get by default.
    ///
    /// Blank/whitespace names normalize to `None`: an empty TOML value means
    /// "off", never a request with `model: ""`.
    pub fn with_summary_fallback(mut self, model: Option<String>) -> Self {
        self.summary_fallback_model = model
            .map(|m| m.trim().to_string())
            .filter(|m| !m.is_empty());
        self
    }

    /// Body for the non-streaming summarize path. Split out from
    /// `chat_once` so the wire shape — above all whether `enable_thinking`
    /// is present — is unit testable without a network.
    ///
    /// `model` is a parameter rather than `self.model` because the fallback
    /// path sends the *same body* under a different model name; keeping one
    /// builder is what makes that guarantee testable.
    fn summary_request(&self, model: &str, prompt: &str, max_tokens: usize) -> ChatRequest {
        ChatRequest {
            model: model.to_string(),
            messages: vec![ChatMessage::user(prompt)],
            tools: Vec::new(),
            max_tokens,
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
            max_tokens,
            temperature: 0.0,
            stream: true,
            enable_thinking: None,
        }
    }

    async fn chat_once(
        &self,
        model: &str,
        prompt: &str,
        max_tokens: usize,
    ) -> std::result::Result<String, LlmError> {
        let body = self.summary_request(model, prompt, max_tokens);

        let mut req = self.client.post(&self.api_url).json(&body);
        if !self.api_key.is_empty() {
            req = req.bearer_auth(&self.api_key);
        }

        let response = req.send().await.map_err(|e| {
            LlmError::new(LlmErrorKind::Transient, format!("LLM request failed: {e}"))
        })?;

        let status = response.status();
        let bytes = response.bytes().await.map_err(|e| {
            LlmError::new(LlmErrorKind::Transient, format!("LLM read body failed: {e}"))
        })?;

        if !status.is_success() {
            let msg = String::from_utf8_lossy(&bytes).into_owned();
            return Err(LlmError::new(
                classify_status(status),
                format!("LLM HTTP {status}: {msg}"),
            ));
        }

        parse_chat_response(&bytes)
    }

    /// One model, `MAX_RETRIES` attempts, backoff keyed on the failure kind.
    /// Returns the *last* error whole so the caller can see what exhausted
    /// the run — that verdict is what decides whether a fallback is warranted.
    async fn chat_with_retries(
        &self,
        model: &str,
        prompt: &str,
        max_tokens: usize,
    ) -> std::result::Result<String, LlmError> {
        for attempt in 0..=MAX_RETRIES {
            match self.chat_once(model, prompt, max_tokens).await {
                Ok(v) => return Ok(v),
                Err(e) => {
                    if !e.retryable() || attempt == MAX_RETRIES {
                        return Err(e);
                    }
                    let backoff = retry_backoff(e.kind, attempt, jitter());
                    let backoff_ms = backoff.as_millis();
                    warn!(attempt, backoff_ms, err = %e.inner, "LLM call failed, retrying");
                    tokio::time::sleep(backoff).await;
                }
            }
        }
        unreachable!("retry loop returns on the final attempt")
    }

    /// Summarize call: retry the primary model, then — only if quota is what
    /// finished it off — spend one attempt on the fallback model.
    ///
    /// The split matters. A 429 is the one failure another model can actually
    /// fix: airouter meters quota *per model*, so a key being rejected on the
    /// primary was serving qwen-flash and deepseek-v3.1 in the same window
    /// (2026-08-04 probe). A 5xx or an empty completion says the backend is
    /// unwell, and shopping around would only double the load it is failing
    /// under — those never fall back.
    async fn chat(&self, prompt: &str, max_tokens: usize) -> Result<String> {
        let err = match self.chat_with_retries(&self.model, prompt, max_tokens).await {
            Ok(v) => return Ok(v),
            Err(e) => e,
        };
        let Some(fallback) = fallback_target(err.kind, self.summary_fallback_model.as_deref())
        else {
            return Err(err.inner);
        };

        warn!(
            primary = %self.model,
            fallback = %fallback,
            err = %err.inner,
            "LLM quota exhausted, falling back"
        );
        ::metrics::counter!("veda_llm_fallback_total").increment(1);

        // One shot, no second retry ladder: the primary already spent ~13s of
        // backoff, and the outbox's own 60/120s schedule is the next line of
        // defence. Two ladders would just hold the worker slot longer.
        match self.chat_once(fallback, prompt, max_tokens).await {
            Ok(v) => Ok(v),
            Err(e) => {
                warn!(fallback = %fallback, err = %e.inner, "LLM fallback model also failed");
                // Surface the primary's error, not the fallback's: "the
                // summary model is out of quota" is the actionable half, and
                // reusing the exact message the no-fallback path stores keeps
                // failed-summary rows comparable across deployments.
                Err(err.inner)
            }
        }
    }
}

/// What kind of failure this is — the axis that decides retry pacing and
/// whether another model could help.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LlmErrorKind {
    /// HTTP 429: the key has no capacity *for this model* right now.
    Quota,
    /// 5xx, transport failure, empty completion — retry the same model.
    Transient,
    /// 4xx (bad request, auth), unparsable body — retrying changes nothing.
    Terminal,
}

#[derive(Debug)]
struct LlmError {
    inner: VedaError,
    kind: LlmErrorKind,
}

impl LlmError {
    fn new(kind: LlmErrorKind, msg: String) -> Self {
        Self {
            inner: VedaError::Internal(msg),
            kind,
        }
    }

    fn retryable(&self) -> bool {
        !matches!(self.kind, LlmErrorKind::Terminal)
    }
}

/// Classify a non-2xx status. Every 429 counts as quota, without reading the
/// body: airouter's carries `type: insufficient_quota`, but a gateway that
/// means "too fast" instead wants the same treatment — back off in seconds,
/// then try a model metered separately.
fn classify_status(status: reqwest::StatusCode) -> LlmErrorKind {
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        LlmErrorKind::Quota
    } else if status.is_server_error() {
        LlmErrorKind::Transient
    } else {
        LlmErrorKind::Terminal
    }
}

/// Backoff before retrying attempt `attempt` (0-based).
///
/// Quota gets the longer seconds-scale ladder *with* jitter, because a 429
/// means several workers hit the same ceiling at once and a fixed schedule
/// would march them back in lockstep. Transient failures keep the original
/// 0.5/1/2s and ignore the jitter argument: nothing there is contended.
fn retry_backoff(kind: LlmErrorKind, attempt: u32, jitter: f64) -> Duration {
    match kind {
        LlmErrorKind::Quota => {
            let base = QUOTA_BACKOFF_BASE_MS * QUOTA_BACKOFF_FACTOR.pow(attempt);
            Duration::from_millis((base as f64 * jitter) as u64)
        }
        _ => Duration::from_millis(BASE_BACKOFF_MS * 2u64.pow(attempt)),
    }
}

/// Uniform multiplier in [0.5, 1.5) applied to the quota backoff.
fn jitter() -> f64 {
    use rand::Rng;
    rand::rng().random_range(0.5..1.5)
}

/// The model to try after a primary run gave up — `Some` only when quota is
/// what exhausted it *and* a fallback is configured. Pure so the trigger
/// condition is tested directly rather than inferred from the call site.
fn fallback_target(kind: LlmErrorKind, configured: Option<&str>) -> Option<&str> {
    match kind {
        LlmErrorKind::Quota => configured,
        _ => None,
    }
}

/// Parse a non-streaming chat completion body into the message content.
///
/// Empty content (after trim) is an error, and a retryable one: summarize()
/// prompts always demand text, so an empty completion means upstream
/// misbehavior. The known mechanism is a reasoning model whose thinking
/// exhausted max_tokens (the 2026-07 empty-abstract incident, where an
/// HTTP 200 carrying content="" was silently persisted); `[llm]
/// summary_disable_thinking` now removes that mechanism on gateways that
/// support the switch, but this guard stays for the backends that don't,
/// for config drift, and for plain upstream flakiness. Retrying may land on
/// a healthy backend; if all retries return empty the task fails loudly
/// instead of storing "".
///
/// `finish_reason` rides along in the message because it tells the two
/// apart at a glance in the logs: `length` = budget exhausted (raise
/// max_summary_tokens, or turn thinking off), anything else = upstream
/// returned nothing despite room to speak.
fn parse_chat_response(bytes: &[u8]) -> std::result::Result<String, LlmError> {
    let parsed: ChatResponse = serde_json::from_slice(bytes)
        .map_err(|e| LlmError::new(LlmErrorKind::Terminal, format!("LLM invalid JSON: {e}")))?;

    let choice = parsed.choices.into_iter().next().ok_or_else(|| {
        LlmError::new(
            LlmErrorKind::Terminal,
            "LLM returned empty choices".to_string(),
        )
    })?;

    let content = choice.message.content.trim();
    if content.is_empty() {
        let reason = choice.finish_reason.as_deref().unwrap_or("unknown");
        // Transient, never Quota: an empty completion is the backend
        // misbehaving on a request it accepted, so retrying the same model is
        // the remedy and a fallback would be treating the wrong illness.
        return Err(LlmError::new(
            LlmErrorKind::Transient,
            format!("LLM returned empty content (finish_reason={reason})"),
        ));
    }
    Ok(content.to_string())
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
        let v = body(&provider(false).summary_request("m", "hi", 8192));
        assert!(
            v.get("enable_thinking").is_none(),
            "enable_thinking must not be sent unless configured: {v}"
        );
        assert_eq!(v["stream"], serde_json::json!(false));
    }

    #[test]
    fn summary_request_disables_thinking_when_configured() {
        let v = body(&provider(true).summary_request("m", "hi", 8192));
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
    fn parse_response_empty_content_reports_finish_reason() {
        // The 2026-07 incident shape: HTTP 200, choices present, content "",
        // finish_reason "length" — thinking burned the whole token budget.
        let body = br#"{"choices":[{"message":{"content":""},"finish_reason":"length"}]}"#;
        let err = parse_chat_response(body).unwrap_err();
        assert!(err.retryable(), "empty content must be retryable");
        assert_eq!(
            err.kind,
            LlmErrorKind::Transient,
            "empty content is the backend misbehaving, not a quota wall — it \
             must not reach for the fallback model"
        );
        assert_eq!(
            err.inner.to_string(),
            "internal error: LLM returned empty content (finish_reason=length)"
        );

        // Whitespace-only content is the same failure after trim.
        let body = br#"{"choices":[{"message":{"content":"  \n "},"finish_reason":"length"}]}"#;
        assert!(parse_chat_response(body).unwrap_err().retryable());
    }

    #[test]
    fn parse_response_empty_content_without_finish_reason_says_unknown() {
        // Gateways that omit the field must not lose the error itself.
        let body = br#"{"choices":[{"message":{"content":""}}]}"#;
        let err = parse_chat_response(body).unwrap_err();
        assert!(err.retryable(), "empty content must stay retryable");
        assert!(
            err.inner.to_string().contains("finish_reason=unknown"),
            "missing finish_reason should read as unknown: {}",
            err.inner
        );
    }

    #[test]
    fn parse_response_empty_choices_is_terminal_error() {
        let body = br#"{"choices":[]}"#;
        let err = parse_chat_response(body).unwrap_err();
        assert!(!err.retryable());
        assert_eq!(err.kind, LlmErrorKind::Terminal);
    }

    #[test]
    fn parse_response_invalid_json_is_terminal_error() {
        let err = parse_chat_response(b"{broken").unwrap_err();
        assert!(!err.retryable());
        assert_eq!(err.kind, LlmErrorKind::Terminal);
    }

    // ── error classification ─────────────────────────────

    #[test]
    fn classifies_429_as_quota() {
        let kind = classify_status(reqwest::StatusCode::from_u16(429).unwrap());
        assert_eq!(kind, LlmErrorKind::Quota);
        assert!(LlmError::new(kind, "x".into()).retryable());
    }

    #[test]
    fn classifies_5xx_as_transient() {
        for code in [500u16, 502, 503, 504] {
            let kind = classify_status(reqwest::StatusCode::from_u16(code).unwrap());
            assert_eq!(kind, LlmErrorKind::Transient, "HTTP {code}");
            assert!(LlmError::new(kind, "x".into()).retryable(), "HTTP {code}");
        }
    }

    #[test]
    fn classifies_4xx_as_terminal() {
        // 400 (malformed request) and 401 (bad key) do not improve with
        // repetition — and must not consume the fallback model's quota either.
        for code in [400u16, 401, 403, 404, 422] {
            let kind = classify_status(reqwest::StatusCode::from_u16(code).unwrap());
            assert_eq!(kind, LlmErrorKind::Terminal, "HTTP {code}");
            assert!(!LlmError::new(kind, "x".into()).retryable(), "HTTP {code}");
        }
    }

    // ── backoff ──────────────────────────────────────────

    #[test]
    fn quota_backoff_is_seconds_scaled_by_jitter() {
        // 1s / 3s / 9s base — seconds because the gateway's limiter is
        // instantaneous concurrency, with no Retry-After to obey.
        for (attempt, base) in [(0u32, 1000u128), (1, 3000), (2, 9000)] {
            assert_eq!(retry_backoff(LlmErrorKind::Quota, attempt, 1.0).as_millis(), base);
            assert_eq!(
                retry_backoff(LlmErrorKind::Quota, attempt, 0.5).as_millis(),
                base / 2,
                "jitter floor at attempt {attempt}"
            );
            assert_eq!(
                retry_backoff(LlmErrorKind::Quota, attempt, 1.5).as_millis(),
                base * 3 / 2,
                "jitter ceiling at attempt {attempt}"
            );
        }
    }

    #[test]
    fn non_quota_backoff_keeps_the_old_schedule() {
        // 0.5/1/2s, unchanged and deliberately un-jittered: nothing here is
        // contended, so spreading retries buys nothing.
        for (attempt, ms) in [(0u32, 500u128), (1, 1000), (2, 2000)] {
            for jitter in [0.5, 1.0, 1.5] {
                for kind in [LlmErrorKind::Transient, LlmErrorKind::Terminal] {
                    assert_eq!(
                        retry_backoff(kind, attempt, jitter).as_millis(),
                        ms,
                        "{kind:?} attempt {attempt} jitter {jitter}"
                    );
                }
            }
        }
    }

    #[test]
    fn jitter_stays_in_band_and_actually_varies() {
        let samples: Vec<f64> = (0..1000).map(|_| jitter()).collect();
        for j in &samples {
            assert!((0.5..1.5).contains(j), "jitter out of band: {j}");
        }
        // A constant would silently defeat the whole point of jittering.
        assert!(
            samples.iter().any(|j| (j - samples[0]).abs() > f64::EPSILON),
            "jitter never changed across 1000 draws"
        );
    }

    // ── fallback ─────────────────────────────────────────

    #[test]
    fn fallback_fires_only_on_quota_and_only_when_configured() {
        assert_eq!(
            fallback_target(LlmErrorKind::Quota, Some("qwen-flash")),
            Some("qwen-flash")
        );
        // Another model is capacity, not a cure: a sick backend or a rejected
        // request stays with the primary and goes to the outbox.
        assert_eq!(fallback_target(LlmErrorKind::Transient, Some("qwen-flash")), None);
        assert_eq!(fallback_target(LlmErrorKind::Terminal, Some("qwen-flash")), None);
        // Unconfigured = today's behaviour, under every kind.
        for kind in [LlmErrorKind::Quota, LlmErrorKind::Transient, LlmErrorKind::Terminal] {
            assert_eq!(fallback_target(kind, None), None, "{kind:?}");
        }
    }

    #[test]
    fn with_summary_fallback_normalizes_blank_names() {
        assert_eq!(provider(false).summary_fallback_model, None, "off by default");
        for blank in ["", "   ", "\t\n"] {
            assert_eq!(
                provider(false)
                    .with_summary_fallback(Some(blank.to_string()))
                    .summary_fallback_model,
                None,
                "blank {blank:?} must mean off, never a request with model=\"\""
            );
        }
        assert_eq!(
            provider(false)
                .with_summary_fallback(Some("  qwen-flash \n".to_string()))
                .summary_fallback_model
                .as_deref(),
            Some("qwen-flash")
        );
    }

    #[test]
    fn fallback_request_swaps_the_model_and_nothing_else() {
        // The fallback re-sends the *same* body under another name — probing
        // confirmed qwen-flash accepts enable_thinking, so the shape does not
        // need to change per model, and a drifting shape would make a fallback
        // failure impossible to reason about.
        let p = provider(true).with_summary_fallback(Some("qwen-flash".to_string()));
        let fallback = p.summary_fallback_model.clone().unwrap();
        let primary = body(&p.summary_request(&p.model, "hi", 8192));
        let secondary = body(&p.summary_request(&fallback, "hi", 8192));

        assert_eq!(secondary["model"], serde_json::json!("qwen-flash"));
        assert_eq!(secondary["enable_thinking"], serde_json::json!(false));
        let mut expected = primary;
        expected["model"] = serde_json::json!("qwen-flash");
        assert_eq!(secondary, expected, "only the model name may differ");
    }

    #[test]
    fn fallback_request_follows_the_thinking_switch() {
        // enable_thinking tracks summary_disable_thinking on the fallback too,
        // exactly as on the primary — including staying off the wire when the
        // deployment never opted in.
        let p = provider(false).with_summary_fallback(Some("qwen-flash".to_string()));
        let v = body(&p.summary_request("qwen-flash", "hi", 8192));
        assert!(
            v.get("enable_thinking").is_none(),
            "unconfigured deployments must not gain the param via fallback: {v}"
        );
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
