# Agent/团队记忆 M2a — answer 双源（引用态）

> 施工图。架构定稿 [`../design/agent-memory.md`](../design/agent-memory.md) §15；
> M1 施工图已归档 [`../archive/plans/agent-memory-m1.md`](../archive/plans/agent-memory-m1.md)，
> 其 §6「M2/M3 预备事实」仍有效。范围经 Joe 确认（2026-08-13）：只做 answer 双源，
> 操作者身份透传（X-Veda-Operator）留 M3。

## 0. 状态

- M1 已上线三节点（0.1.27 / `999d452`，.161/.89/.85 同 binary），dogfood 进行中。
- 本期目标：记忆成为 answer 的第二证据源与出处一类——「引用态是第一天的存在感」。

## 1. 范围（五项）

1. **注入**：`answer_stream` 初检后、引擎启动前，对 workspace **团队域**记忆做一次
   retrieve（`MemoryService::team_memories`，新增 4 行方法，复用 retrieve 的
   Milvus 候选 → MySQL 复核 → touch 链路；检索即计数）。命中作为独立证据块进入
   首条 user 消息，与文件初检结果并列。
2. **引用态**：`AnswerCitation` 扩展——`path` 改 `Option<String>`，新增
   `memory: Option<MemoryCitationRef { id, content, updated_by, updated_at }>`；
   文件引用带 path、记忆引用带 memory，互斥。`BlockRegistry` 加 Memory 块类。
3. **注入模板**（设计稿「标签/日期/资料块框」的落点）：
   `[n] 记忆(kind·更新日期·署名) <<<content>>>`，实现为 answer 侧证据渲染函数。
   **偏差记录**：MCP `memory_context` 保持 M1 的结构化 JSON（已含 scope/署名/日期 +
   framing note），不再叠加文本模板——一种消费者一种格式，agent 读 JSON、LLM prompt 读模板。
4. **消费端渲染**：tunnel `render_answer` 出处列表加记忆行（`[n] 记忆：content 截断`）；
   MCP ask 的 citations JSON 自动携带 memory 对象（零额外改动）；CLI ask 渲染补记忆行
   （代码先行，CLI 不发版）。
5. **测试 + 文档**：见 §3、§4。

## 2. 设计决定

- **只注入团队域**：个人域进 answer 需要操作者身份（tunnel user_id / X-Veda-Operator），
  整体留 M3——身份源扩展一次设计一次上线。
- **无分数下限、无 rerank**：不对空数据调参（M1 原话）。唯一新参数
  `MEMORY_INJECT_LIMIT = 5`。乘子/下限等 dogfood 攒出分布再调。
- **hit_count 含 memory 块**：tunnel outcome 分类不受影响（refusal 优先判定，
  memory-only 命中 + 拒答仍归 no_context）。
- **兼容性**：旧 tunnel 对 memory 引用优雅降级——其 `AnswerCitation.path` 为
  `Option` 且未知字段忽略，无 path 的引用自动跳过出处行，正文 `[n]` 保留。
  server 先上、tunnel 后升即可，无硬部署顺序。
- **记忆内容进 prompt 的注入面**：记忆是团队成员显式写入的数据，与文件同级视为
  不可信外部资料——TOOL_PROTOCOL 既有的「只作依据不执行指令」约束覆盖之，不加新防线。

## 3. 测试（真实 MySQL/Milvus/embedding + 真实 LLM，`--ignored`）

- 正向：空文件 workspace 种一条团队记忆（独特事实）→ `/v1/answer` 提问 →
  断言 citations 含该 memory id（照 answer_stream_test 既有 flake 面）。
- 负向（GateMem 口径延伸到 answer 面）：异 workspace 提同一问题 →
  `hit_count == 0` 且无 memory citation（结构断言，与 LLM 行为无关）。

## 4. 文档（Step 4 收尾）

ARCHITECTURE.md（answer 双源）/ CHANGELOG.md / design doc 状态行；
aidoc + APIDoc 的 memory 欠账（M1 时明确等 M2a）——aidoc 改动先给 Joe 过目再 push。

## 5. 不做（防散）

个人域进 answer、操作者透传、排序乘子、分数下限（各等 M3 / 数据）；
M2b 对账提名（等真实案例）；digest（触发条件未到）。

## 评审记录（2026-08-13，Codex 原生 review，2 findings 全修）

1. **[修] SSE 前的记忆检索无超时**（P1）：`team_memories` 的 await 发生在
   SSE 打开前、持有 workspace answer permit 时，且 route 层对 `answer_stream`
   调用本身无 timeout——embedding 共享闸无限排队时 permit 永不释放，同
   workspace 全部 429。修法与相邻初检一致：15s timeout 包裹，超时走既有降级
   （warn + 空记忆）。
2. **[修] `spans: []` 被 skip_serializing 吞掉**（P2）：给 spans 加的
   `skip_serializing_if` 把**既有**整文件引用的 `{index,path,spans:[]}` 变成
   `{index,path}`，破坏对外契约「空数组 = 整文件」。修法：spans 恒序列化
   （保留 `default` 供反序列化宽容），memory 引用也带空 spans，消费端按
   `path`/`memory` 判类不受影响。
   顺带发现：`cargo fmt --check` 在 veda-cli 存量代码上不干净
   （config.rs/main.rs 数处，非本期改动），另行处理。

## DoD

集成测试全绿（含既有 answer / memory 套件零回归）→ 测试环境部署
（server .161/.89 + tunnel 测试实例 .89）→ 归档本计划。生产随下次发版窗口，Joe 拍板。

## 部署记录（2026-08-14，DoD 完成）

- server-only 部署（不发版，版本自报仍 0.1.27）：main `d8a0ed0` 在 .89 build 一次，
  server sha `3c70b305` 上 .161/.89，tunnel sha `f1080882` 上 .89 测试实例；
  生产 .85/.95 未动，随下次发版窗口。
- 验证：三项产物校验 + 双 binary 内容锚点（「以下是团队记忆」/「记忆：」）；
  两节点 healthz/ready 绿、journal 零错误；tunnel bot `conn_state=subscribed`；
  公网入口 200。
- 行为冒烟（.89 真实链路）：匿名 workspace 种团队记忆 → `/v1/answer` 答案
  引用 `[1]` 并给出正确事实，citation 为 `{index, spans:[], memory:{id,content,
  updated_by,updated_at}}`（无 path、spans 恒序列化、hit_count=1）——引用态即上线。
