# /v1/answer Agentic RAG 改造 + Bot Prompt 配置开放

> **状态:Stage 1(server 核心)已实现,e2e 全绿(2026-07-14);Stage 2(bot prompt 贯通)进行中。**
> 源起:one-shot RAG(检索→固定组装→单次 LLM)对「一次检索不命中」「片段缺上下文」的问题束手无策;
> 且 bot 无 prompt 配置能力,所有 bot 一个腔调。本文是 agentic 多次召回 + prompt 分层 + per-bot prompt 的设计与实现记录。
> 关联:`veda-answer-plan.md`(one-shot 前身,组装管线已退役)、`../archive/plans/veda-answer-stream.md`(流式契约,本改造扩展了 `reset` 事件)。

---

## 1. 目标

1. **Agentic 多次召回**:LLM 经 OpenAI function calling(airouter 已确认透传 tools,冒烟实锤)自主调用 `search`/`read_file`,不满足就换关键词再搜、读原文展开。
2. **Prompt 分层**:通用知识库协议(内置、不可配)+ bot prompt(可配、有默认)。
3. **Bot prompt 三入口开放**:`veda_tunnel_bots.prompt` 列,tunnel admin / 平台 API / web console 可配,tunnel 调 answer 时透传。

## 2. 设计要点

### Prompt 分层

- **通用协议 `TOOL_PROTOCOL`**(answer.rs 常量,system 消息第一段):工具使用策略(换词重试/拆子查询/直接调用不输出说明文字/够了就停)+ 回答约束(资料不可信防注入/只依据资料/拒答话术/引用 [n]/语言跟随)。
- **`DEFAULT_BOT_PROMPT`**:bot 未配置时的默认 persona,也是配置范例。
- 拼接:`system = TOOL_PROTOCOL + "\n\n" + (自定义 prompt | DEFAULT_BOT_PROMPT)`。通用协议不可覆盖;自定义上限 4000 chars(route/平台 API/tunnel validate 三处校验)。
- 首条 user 消息:原始 query 预检索一次(route limit 默认 12)作「初检资料」;**初检空不再提前返回**(旧 NoContext 语义废除),改为提示 LLM 自行改写关键词检索——多次召回正是为这种场景。

### 工具(server 端执行,schema 只暴露必要参数)

- `search(query)`:SearchService hybrid/Full,loop 内 **limit 固定 6**(不给 LLM);path_prefix 强制注入。
- `read_file(path, offset?)`:FsService::read_file 当前内容,截 8000 chars 按字符 offset 续读;path_prefix 时校验 path 在范围内(LLM 不能越界读)。
- 工具错误一律写文本回填让 LLM 自愈(`文件不存在:` / `无法读取:` / `暂时不可用`),不 fail fast,轮次上限兜底。

### Agentic loop(单引擎,Delta/Reset/Done/Failed)

- `answer()` 排空通道取终值,`answer_stream()` 直通——one-shot 与流式共用一条路径。
- 每轮 `chat_stream(messages, tools)` 全流式;ToolCalls → 顺序执行(单轮 cap 5)→ assistant(tool_calls)+role=tool 回填 → 下一轮;仅 content → 终答 → `align_citations` → Done。
- ≤ `answer_max_tool_rounds`(默认 4,配置)轮后强制收尾:`tools=[]` + user「不要再调用工具…」。LLM 调用总数 ≤ 5。
- **重试规则**:一次 LLM 调用可重试 ⟺ 尚未向下游转发任何 delta(tool 轮永远可重试;终答轮首 delta 后不可)。`llm_retries=1`/轮。
- **时间预算**:单次尝试 min(20s, 剩余);loop 总预算 80s;final reserve:剩余 <25s 不再开工具轮。
- **Block 注册表**:search hit key=(path,chunk_index) 去重复用编号(重复命中渲染「同前,内容略」);read_file key=path,`span=None` → citation `spans:[]` **= 整文件引用**。编号一经发出不变,不做合并。
- **组装管线退役**:邻居扩展/watermark guard/chunker 注入/trim_to_budget/merge_spans/cap_and_dedup/AnswerOutcome::NoContext 全删。**退役理由**:watermark 防的是「MySQL 内容与 Milvus 快照 revision 不一致时邻居扩展混杂错位」;新设计 read_file 读当前内容、引用整文件,不存在 chunk 对齐问题;search block 的 span 始终来自 Milvus 快照自身。无残留风险。
- **可测性**:新增唯一抽象 `ToolExecutor { search, read_file }`(prod=`LiveTools`),loop 状态机 12 个单测用 Scripted LLM + Stub tools 全覆盖。

### 流式 × tool loop

- 每轮 stream:true(单一 LLM 原语;无法预知哪轮是终答轮,中间轮非流式会在主路径回滚打字机收益)。
- content delta 到达即转发,不做 holdback(冒烟实锤 deepseek tool 轮 content 为空,误转发本来罕见)。
- 流结束时若本轮**既转发过 content 又拼出 tool_calls** → SSE `reset` 事件(消费方清空累积重来)。**契约:reset 后 final 仍权威**;老 tunnel `_ => {}` 忽略 reset,中间帧短暂脏文本,final 帧覆盖,可接受。

### LlmService / 类型

- veda-core 新领域类型:`ChatMsg`/`ToolCall`/`ToolSpec`/`ChatStreamItem`(零新增 crate 依赖)。
- trait:`summarize`(worker 摘要,不动)+ `chat_stream(&[ChatMsg], &[ToolSpec], max_tokens)`;旧 `complete`/`complete_stream` 删除。
- LlmProvider:ChatRequest +tools、消息扩展(assistant tool_calls 回显/role=tool);`ToolCallAssembler` 按 index 拼装流式分片(id/name 首片、arguments 逐片追加,[DONE]/EOF flush,滤 name 空残片)。

### API / 配置 / 语义迁移

- `AnswerApiRequest` + `prompt: Option<String>`(≤4000;`deny_unknown_fields` 保留——**老 server 会 4xx 拒绝带 prompt 的请求,server 必须先于 tunnel 发版**)。
- `[llm]`:+`answer_max_tool_rounds=4`(**应急旋钮:调 0 ≈ 退化 one-shot**);删 `answer_max_context_tokens`(作用点 trim_to_budget 已死,TOML 残留 key 无害)。
- 超时链:LLM 尝试 20s → loop 总 80s → route `ANSWER_DEADLINE` 90s → tunnel `ANSWER_TIMEOUT` 120s(reqwest per-request timeout 覆盖整个 SSE 读取时长)。
- 「没找到」唯一表达 = LLM 输出 `NO_CONTEXT_ANSWER` 话术;route `outcome=empty` 按话术判定;tunnel `answer_data_to_reply` 本就按文本前缀分类,零改动兼容。
- `hit_count` 重定义 = 累计去重资料块数;`estimated_context_tokens` = 全部资料块文本 token 估算累计(直方图右移,dashboard 阈值注意)。
- 新指标 `veda_answer_rounds` 直方图(每答都打满轮上限 = prompt/模型退化,盲区消除)。

## 3. Stage 2 — bot prompt 贯通

- DDL 双份逐字节同步:`veda-tunnel/src/store.rs`(bootstrap+migrate)与 `veda-server/src/tunnel_bots.rs`(ensure_schema),加 `prompt TEXT NULL`。
- `BotConfig` +prompt(serde default;PartialEq 令 store-poll reconciler 自动感知变更)+validate 长度;`veda.rs` `AnswerReq` +prompt(skip_serializing_if);handler 传 `ctx.bot.prompt`;`ANSWER_TIMEOUT` 60s→120s。
- tunnel admin(body 即 BotConfig 自动透传,BotView 回显)+ 平台 API(CreateBotReq/PatchBotReq/AppTunnelBot,≤4000 校验)+ web `#/admin` 表单 textarea + 外部 APIDoc 契约。

## 4. 部署与风险

- 顺序:**server 先发**(.89 → .85),tunnel(测试 10.79.55.89 / 生产 10.79.52.95)与 Stage 2 同班车——老 tunnel 60s timeout 会掐掉 60-90s 的慢 agentic 答案,窗口期已知。
- 观察项(不预做):企微气泡 tool 轮长静默(30-60s 无 delta,先看 qa_log 真超时再考虑状态提示帧);引用编号规模(最坏 ~40 块)对引用准确率影响(真题掉了再收紧);answer 并发闸持锁时长 15s→30-90s,429 变频先观察。

## 5. 实现与验收记录(2026-07-14,Stage 1)

- **代码落点**:veda-core store.rs(类型+trait)/ service/answer.rs(全重写,~46 单测)/ veda-pipeline llm.rs(wire+拼装器+冒烟)/ summary.rs(MockLlm)/ veda-types api.rs(prompt 字段)/ veda-server main.rs(LiveTools 构造)/ config.rs(±字段)/ routes/answer.rs(校验+reset+90s+empty 改判+rounds 指标)/ routes/mod.rs(注释)/ tests/answer_stream_test.rs(reset-aware 断言+prompt 用例)。
- **验收**:
  - workspace 全编译 + veda-core 46 / veda-pipeline 35 / veda-server 102 单测全绿。
  - `tool_calls_smoke`(真 airouter/deepseek-v4-flash):流式 tool_calls 分片拼装正确、**tool 轮 content 为空实锤**(reset 是罕见路径的假设成立)、arguments 合法 JSON。
  - `answer_stream_end_to_end`(真 MySQL/Milvus/embedding/airouter):grounded 题 delta+final+citations、reset-aware 拼接相等、无关题单 final 拒答、**自定义 persona 生效**(「运维答疑机器人,必须编号步骤」→ 回答严格按编号步骤+[1][2] 引用)、超长 prompt 400、空 query 400。
