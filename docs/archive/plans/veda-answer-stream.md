# /v1/answer 流式（T2 / answer-plan P1）

> 📌 **补充(2026-07-14 agentic 重构)**:SSE 契约新增 `reset` 事件(丢弃已累积 delta,罕见的说话后调工具轮);`final==concat(deltas)` 在 reset 发生时需按「reset 清空」口径理解,final 仍权威。见 `veda-answer-agentic.md`。
>
> 📌 **补充(2026-07-15 tool 进度透出)**:SSE 契约再增 `tool` 事件 `{"name","detail"}`(工具执行前发出,detail=检索词/文件路径,≤60 字符,不含工具结果),纯进度提示可丢弃;tunnel 渲染为「🔍 正在检索:…」/「📄 正在查阅:…」状态行帧(与 interim 共享 1s 节流),覆盖 placeholder→首个 delta 之间的工具轮静默段。旧 tunnel 忽略未知事件,兼容。

> **状态：已上线（2026-07-14）**。DoD 全过：SSE 解析 3 单测 + drive_stream 4 单测 + 非流式 28 测零变化 + tunnel 34 测 + 真实 e2e（多 delta/final==拼接/citations/no_context 单帧/400）；.89 冒烟 curl -N 实锤逐段吐字后 .85/.95/.89-tunnel 全部署。待真机体感确认（Joe）。

> 动机（qa_log 实测数据）：answer 总耗时 97% 在 LLM 生成（检索 0.2s、生成 7-11s @ ~80-115 tok/s，与模型选择关系不大——基准见 tunnel-directions 讨论）。流式不减总时长，但首 token 1-2s 即出，企微体感从「占位干等 7 秒」变「即时逐段出字」。
> 原则：**终帧权威**——citations 只能在全文就绪后对齐（`align_citations` 扫 `[n]` 标记），所以增量帧只有文本，最后一帧携带完整 `AnswerApiResponse`；消费方以终帧为准（中途丢帧无影响）。

## 链路

```
airouter (OpenAI SSE, stream:true)
  → LlmProvider::complete_stream（SSE 行解析 → delta 文本流）
  → AnswerService::answer_stream（prepare 复用非流式前半 → 转发 delta → 流毕 align → Done）
  → POST /v1/answer/stream（axum SSE：event delta/final/error；挂 TimeoutLayer 外）
  → tunnel veda.rs answer_stream（SSE 客户端）
  → handler 节流刷新企微 stream 帧（≥1s/帧，全量替换语义），Final 时 render_answer + finish
```

## 决策

| 点 | 决定 | 理由 |
|---|---|---|
| API 形状 | 独立端点 `POST /v1/answer/stream`（SSE），入参同 `/v1/answer` | 响应类型不同（event-stream vs JSON）；同 events.rs 先例挂超时层外 |
| SSE 事件 | `delta {"text"}` / `final {完整 ApiResponse<AnswerApiResponse>}` / `error {"error_code","error"}` | 终帧权威；HTTP 已 200 后的失败只能靠 error 事件表达 |
| 前置错误 | 400/401/429/501 在 SSE 开始前按普通 HTTP 返回（复用非流式检查；429 permit 持有到流结束） | 客户端好处理 |
| 重试语义 | 首 delta 前的失败可重试（沿用 attempt 次数）；已发 delta 后失败 → error 事件，不重试 | 内容已流出，重试会重复 |
| 超时 | 首 token 与帧间空闲复用 `llm_attempt_timeout`(20s)；整流 45s 兜底（server 层逐事件 recv 计时） | 不加新参数 |
| tunnel 节流 | 距上帧 ≥1s 且有新增才刷企微帧（全量替换）；流式刷新是否计入 30 条/min 官方未说明，1s 节流 = 单答 ≤10 帧，安全 | 协议调研结论 |
| tunnel 降级 | `/v1/answer/stream` 404/405（老 server）→ 自动回落非流式 `answer()` | server/tunnel 部署顺序自由 |
| core 返回形态 | `answer_stream(self: Arc<Self>, ..) -> mpsc::Receiver<AnswerStreamEvent>`（内部 spawn 驱动） | core 不依赖 axum；spawn 需 'static，Arc receiver 自然 |
| LlmService trait | 加 `complete_stream(&self, prompt, max_tokens) -> Result<BoxStream<Result<String>>>` | mock 实现者同步补 |

## DoD

1. server 集成测试（真 airouter）：POST /v1/answer/stream 收到 ≥2 个 delta + 1 个 final，final 的 answer == delta 拼接（trim 后），citations 非空
2. 非流式 `/v1/answer` 行为零变化（现有测试全绿——prepare 提取是纯重构）
3. SSE 解析器单测（pipeline + tunnel 各自：分块切割/[DONE]/坏行降级）
4. core answer_stream 单测（mock 流式 LLM：delta 序列 → Done 含对齐 citations；中途失败 → Failed）
5. tunnel 节流纯函数单测
6. 真机：企微答案逐段出现（Joe 验体感），qa_log 照常落行（outcome/latency 以 final 为准）
7. 部署：.89 server → .85 server → .95/.89 tunnel

## 不做

多路复用单连接、断点续传、`<think>` 展示（后评估）、非企微消费方的 SSE 文档化（APIDoc 等 AI 工作台有需求再补）。
