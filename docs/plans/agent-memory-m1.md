# Agent Memory M1 施工图

> **状态：已拍板开工（2026-08-12），未动代码。**
> 架构与论证见 [`../design/agent-memory.md`](../design/agent-memory.md)（18 节提案 + 两批八项拍板），
> 本文只管 M1 怎么落地：范围、数据模型细化、施工步骤、DoD。
> 完工后按协议归档并在 `plans.md` 更新索引。

---

## 0. 范围，与设计稿的偏差

**M1 = 原子记忆最小可用**：`veda_memories` + `veda_principals` 两张表、共享 Milvus
collection、`MemoryService`、5 个 MCP 工具 + REST、归属分域 + key 源身份解析、save 返回近邻。

对 design doc §16 的两处调整（已与 Joe 对齐 2026-08-12）：

1. **「Milvus 只产候选、MySQL 复核」从 M2 提前到 M1**。M1 的 DoD 是 GateMem 两断言，
   其中「删后检出 = 0」直接依赖读时复核——不做复核，Milvus 双写窗口里就能检出已删内容。
   `expires_at` 过滤和 `last_used_at` 触点也顺手 M1 落，排序权重调优才留 M2。
2. **身份解析 M1 只做 key 源**（`ensure_principal('key', key_id)`）。GatewayUser、
   企微 user_id 分别在 M2 平台面、M3 tunnel 摄入上线时再挂——M1 唯一消费者是 MCP。

M1 性质两条，决定了它没有开关：

- **零 LLM 依赖**：save 近邻和 topic 缺省归属只用 embedding；embedding 是必配项，
  所以记忆功能无 feature gate、零新配置键。
- **零风险部署**：新表 `CREATE TABLE IF NOT EXISTS` + 新 collection lazy init，
  对现有功能零影响，随下次发版直接上三节点。

---

## 1. 数据模型

### 1.1 MySQL 两张表

```sql
CREATE TABLE IF NOT EXISTS veda_memories (
    id BIGINT AUTO_INCREMENT PRIMARY KEY,        -- 数字 id：[mem:123] 引用格式（digest/UI/引用校验）
    scope_type VARCHAR(16) NOT NULL,             -- 'workspace' | 'principal'，归属分域
    scope_id VARCHAR(36) NOT NULL,
    origin_workspace_id VARCHAR(36) NULL,        -- 个人域第二刀：项目笔记带、随身偏好空（默认值规则 design §4.2）
    topic VARCHAR(128) NULL,                     -- 记忆的「目录」，M3 digest 的分组键，M1 起即填
    kind VARCHAR(16) NOT NULL,                   -- 'fact'|'preference'|'decision'|'procedure'|'derived'
    content TEXT NOT NULL,
    content_hash CHAR(64) NOT NULL,              -- sha256(content) 原样，不归一化
    source_ref JSON NULL,                        -- 证据指针，形状见 1.2
    expires_at TIMESTAMP NULL,
    last_used_at TIMESTAMP NULL,                 -- 检索命中即 touch，排序下沉的唯一信号
    created_by VARCHAR(36) NOT NULL,             -- principal id，署名即治理
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    updated_by VARCHAR(36) NOT NULL,
    updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
    UNIQUE INDEX uq_scope_hash (scope_type, scope_id, content_hash),  -- 精确去重压 DB 层，并发写天然挡
    INDEX idx_scope_topic (scope_type, scope_id, topic)               -- 服务浏览/digest 选材，左前缀可单用
);

CREATE TABLE IF NOT EXISTS veda_principals (
    id VARCHAR(36) PRIMARY KEY,                  -- uuid，跟主流表惯例（要进 scope_id/created_by）
    kind VARCHAR(16) NOT NULL,                   -- 'human' | 'agent'；key 源默认 'human'，M1 不参与逻辑
    source VARCHAR(16) NOT NULL,                 -- 'gateway' | 'wecom' | 'key'
    external_id VARCHAR(128) NOT NULL,           -- key 源 = veda_workspace_keys.id
    display_name VARCHAR(128) NULL,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    UNIQUE INDEX uq_source_ext (source, external_id)   -- 首见 lazy 建，照 ensure_account 套路
);
```

- 零状态列 / 关系皆列 / 分域即过滤列，论证见 design §4 §6，不复述。
- 时间比较（expires_at 过滤等）统一 SQL 侧 `NOW()` 完成，不在 Rust 侧拿本地时钟比
  ——吸取 2026-08-04 outbox 限速 TIMESTAMP 时区从未生效的教训。
- digest 表（`veda_memory_digests`）M3 再建，无兼容包袱。

### 1.2 source_ref 形状（M1 定死，不自由发挥）

```json
{"files": ["/docs/deploy.md"],   // 同 workspace 文档
 "qa_log_ids": [123],            // 指向 veda_tunnel_qa_log（M3 摄入的锚，摄入路径必填）
 "memory_ids": [88, 90]}         // derived 的支撑（M3 画像用）
```

- 可空：主动写不强制证据（强制会劝退写入），靠工具 description 引导「有出处就带上」。
- M3 自动摄入 source_ref **必填** qa_log_ids，并加 `"ingest": "qa_log"` 标记
  ——将来批量清理自动抽取的低质量记忆时有据可筛。
- **episodes 是角色不是表**：每个摄入源用自己的原生存储当原始层（qa_log 就是现成的
  episode 表），memory 只存指针。出现「没有原生落库的摄入源」时才建通用 episodes 表。
- 校验只在信任边界：合法 JSON + 大小上限 + 已知键类型，不深校验指针有效性（M1）。

### 1.3 Milvus：纯向量索引，不是数据副本

```
collection "veda_memories"（全租户共享一个，学 fs 侧 veda_summaries，不学 db 侧 per-ws）
  id                   VARCHAR pk     ← 与 MySQL 同 id（字符串化的 BIGINT）
  scope_type           VARCHAR   ┐
  scope_id             VARCHAR   │ 域过滤标量，仅此三个
  origin_workspace_id  VARCHAR   ┘（随身偏好存空串——Milvus 标量过滤对 NULL 不友好）
  vector               FLOAT_VECTOR（dim 随 embedding 配置，AUTOINDEX COSINE，纯 dense 无 sparse）
```

与 `veda_summaries` 模式的刻意偏差：**content 不进 Milvus**（summaries 存了）。
读路径定死「Milvus 只产候选 id → MySQL 取权威」，Milvus 里的 content 没有消费者，
存了反而是危险冗余（update 后同步失败窗口里就是一份旧文本）。kind/topic/expires_at
同理不进——打折、过滤全在 MySQL 复核侧。

由此得到的总性质（安全设计的根）：**Milvus 任何失败只影响召回率，不影响正确性**。
漏写→搜不到（outbox 补）；残留→复核查无此行自动消失；过滤出 bug 混进跨域 id→复核
查询自带 scope 条件二次挡。GateMem 断言可测，靠的就是 Milvus 不在信任链上。

建 collection 照 `milvus.rs::init_summary_collection`（milvus.rs:321 一带）。

### 1.4 排序：相似度参与的在代码里，不参与的在 SQL 里

- **检索路径**（search/context）：排序在应用层——相似度分只在应用层手里，候选仅
  2K 条量级。`final = similarity × recency乘子(last_used_at) × kind乘子(derived 打折)`，
  只调序不扩召回（mem0 读时融合结构）。**M1 只做 last_used_at 埋点，排序先纯相似度**
  ——乘子曲线等 dogfood 攒出真实分布后 M2 调，不对着空数据调参。
- **非检索路径**（M3 digest 选材、M4 浏览页、admin）：纯 SQL `ORDER BY`，
  `idx_scope_topic` 左前缀服务，每域几百条量级 filesort 无感。

---

## 2. 读写路径与失败语义

### save(content, kind, topic?, source_ref?, expires_at?, scope?)

```
1. 解析 scope → (scope_type, scope_id)     P 由服务端从身份解析，不收客户端入参
2. embed(content)                          交互优先闸；失败 → 整个 save 报错（agent 重试安全）
3. Milvus 同域近邻 top-N → id → MySQL 取全行（近邻卡片：内容+署名+日期+topic）
4. topic 缺省 = top-1 近邻的 topic（相似度过阈值才继承，初值拍 0.75，M1 可调）
5. MySQL INSERT                            撞 uq_scope_hash → 返回已有行，幂等语义
6. Milvus upsert(id, 标量, vector)          失败 → 入 outbox MemorySync 重试（background 优先级）
7. 返回 { id, neighbors: top-3 }           近邻引导 agent「改旧条还是新写」
```

重试自洽性：save 整体失败后 agent 重发，步骤 5 撞唯一键当成功、继续补 6，无重复行。

### search(query, …) / context()

```
1. embed(query)                            交互优先闸
2. Milvus filtered ANN，over-fetch 2×limit  （复核会筛掉删除/过期候选，不补量会不足 K）
   context 的域合并是一条表达式一次查询：
   (scope_type=='workspace' && scope_id==W)
   or (scope_type=='principal' && scope_id==P && (origin=='' || origin==W))
3. MySQL 复核：WHERE id IN (...) AND <同一 scope 条件> AND (expires_at IS NULL OR expires_at > NOW())
   —— 权威内容以此为准；scope 条件必须重复带上（双保险，纪律见 design §4.1）
4. 代码里排序（M1 纯相似度）、截 K
5. touch last_used_at：单条批量 UPDATE，fire-and-forget
```

### update(id, …) / delete(id)

- update：MySQL UPDATE（校验目标行在调用方可写域内）→ re-embed → Milvus upsert 同 id 覆盖；失败入 outbox。
- delete：MySQL DELETE → Milvus delete by pk；Milvus 失败**不影响正确性**（复核挡），
  outbox 顺手补，admin reconcile 兜底（照 `reconciler.rs::reconcile_summaries` 加 memories 一段，M1 可选做）。
- 权限跟域走：个人域 = 本 principal，团队域 = 本 workspace 全员；由读写原语的 scope 条件天然保证，无新增权限动作。

### 身份与 scope 落库映射

- principal 解析：请求 → `wk_` key → `AuthWorkspace.key_id`（新增字段，`resolve_ws_key`
  单查询顺手带出）→ `ensure_principal('key', key_id)` lazy upsert。
- `team` → `(workspace, W)`；`mine` → `(principal, 操作者P)`；`self` → `(principal, agentP)`。
  M1 key 源下 mine 与 self 落点相同，语义分化等 M2/M3 多身份源接入后自然出现。
- scope 缺省 `mine`；origin 缺省按 kind：fact/decision/procedure 锁当前 W，preference 空（design §4.2，永不报错）。

---

## 3. 施工步骤（4 步，每步独立 commit）

### Step 1 — types + store（地基）

| 动什么 | 哪里 |
|---|---|
| `MemoryScope`/`MemoryKind`/`Memory` 领域类型 + API DTO + 错误码 | `veda-types` |
| 两张 `CREATE TABLE` 进 stmts 数组 | `veda-store/src/mysql/schema.rs` |
| `MemoryStore` trait（insert/update/delete/get_by_ids/ensure_principal/touch_last_used…） | `veda-core/src/store.rs` |
| MySQL 实现，一文件一职责 | `veda-store/src/mysql/memory.rs`（新） |
| `init_memory_collection` + upsert/delete/search_memories | `veda-store/src/milvus.rs` |
| store 层真实依赖集成测试 | `veda-store/tests/` |

### Step 2 — veda-core MemoryService

- `service/memory.rs`（新），照 `SearchService`（service/search.rs:42）样板：
  struct 持 `Arc<dyn MemoryStore / VectorStore(或专用 trait 方法) / EmbeddingService>`。
- §2 全部读写路径 + scope/origin 解析逻辑；mock 单测（复用 `tests/mock_store.rs` 模式）。
- 新增 `OutboxEventType::MemorySync` 变体（`veda-types/src/types.rs:98` 枚举 +
  `worker.rs` dispatch match + `outbox_event_label`）。

### Step 3 — veda-server 接入面

- `auth.rs`：`AuthWorkspace` 加 `key_id` 字段（resolve_ws_key 已 JOIN，零成本）。
- `state.rs` 加 `memory_service`；`routes/memory.rs`（新）：
  `POST /v1/memory`、`PATCH/DELETE /v1/memory/{id}`、`GET /v1/memory/search`、
  `GET /v1/memory/context`——`AuthWorkspace` fs-only，写动作过 `require_write()`
  （read-only `wk_` 可 search/context）。`routes/mod.rs` merge。
- `routes/mcp.rs` 5 个工具，每个动四处：`tool_specs()`（memory_save/update/delete
  为 `readOnlyHint:false`）、`run_tool()` match、handler、`tool_metric_label()` 白名单；
  `initialize_result()` instructions 重写（加记忆引导）。
- **mcp.rs:991 的「7 个工具且全 readOnlyHint=true」硬编码断言必须重构**为分组断言
  ——memory 首次打破全只读格局；`tests/mcp_http_test.rs` 的 tools/list 断言同步。
- **工具 description 是一等交付物**：scope 判据（「知识关于谁，不问谁写的」）、
  抽取三原则（宁缺毋滥/独立可懂/先合并）、「先看近邻再决定 update 还是新写」
  全靠 description 引导，无服务端强制。落地时文案单独过一轮 review。

### Step 4 — 集成测试 + 文档

- `veda-server/tests/memory_test.rs` mega-test（真实 MySQL/Milvus/embedding，
  `NO_PROXY='*' cargo test -- --ignored --test-threads=1` 惯例）：
  - CRUD 往返、save 近邻返回、topic 继承、唯一键幂等
  - scope 三档落域正确；read-only `wk_` 能读不能写
  - **GateMem 断言 1（越权 = 0）**：B 的 key 检不出 A 个人域内容
  - **GateMem 断言 2（删后 = 0）**：特意制造 Milvus 残留窗口（删 MySQL 行、留 Milvus
    向量）断言复核挡住——这条测的就是 §0 调整 1 的价值
  - MCP 面 5 工具往返 + tools/list 目录
- 文档：ARCHITECTURE.md 新节、CHANGELOG `[Unreleased]`、plans.md 索引、
  design doc 状态行；aidoc/APIDoc **不动**（等 M2 answer 双源 / 平台面再说）。

单人量级 4–5 天成型。

---

## 4. DoD

1. 单测 + mega 集成测试全绿（真实依赖）。
2. GateMem 两断言进集成测试且为确定性断言（非概率）。
3. 部署 .161，Joe 的 coding agent 经 MCP 真用起来（dogfood 即验收）
   ——重点观察工具 description 引导下 agent 实际写出的记忆质量与 scope 选择。
4. 文档四件套更新。

## 5. M1 明确不做（挡镀金）

CLI `veda memory` 子命令（agent 用 MCP、人等 M4 浏览页）；`[memory]` 配置开关（零依赖
天然可用）；Milvus sparse/BM25；org 域；digest 表与编译；画像；对账提名；staging/候选表
（= 审批队列还魂，M3 用「自动抽取落个人域」替代其功能）；episodes 表；编辑历史；
排序乘子调参（埋点先行）；reconcile memories 段（可选，不阻塞）。

## 6. M2/M3 预备事实（本期不做，防丢）

- **M2 建议拆两半**：M2a = answer 双源 + 注入模板（引用态是「第一天的存在感」，尽快）；
  M2b = 对账提名（等 dogfood 攒出真实矛盾案例，避免对空数据设计）。
  M1 与 M2a 之间留 dogfood 检查点。
- **M3 tunnel 摄入**：提问人 user_id 目前只活在 tunnel 进程内——server 侧
  `AnswerApiRequest`（veda-types/api.rs:273）无此字段且 `deny_unknown_fields`，
  加可选 `user_id` 后**部署顺序必须先 server 后 tunnel**（veda.rs:61 注释已记此约束）。
- qa_log 表**没有 workspace_id 列**，摄入归属要经 bot_id 反查（tunnel_bots.rs 已有先例）。
- qa_feedback 表（UNIQUE(feedback_id,user_id)）是现成的「只摄入被点赞问答」过滤源。

## 7. 部署注意

- 三节点随下次 server 发版直接带上（新表新 collection，无人调则无数据）。
- **生产 .85 的 Milvus 建 collection 权限是已知坑**（当初建表踩过）：上生产前确认
  账号有 CreateCollection 权限，或提前手工建好 `veda_memories` collection。

## 8. 偏差记录

> 实现过程中偏离本 plan 的地方记在这里（协议要求），完工归档时随文带走。

（暂无）
