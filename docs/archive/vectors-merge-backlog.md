# vss merge — backlog (未修问题清单)

> 来源：HEAD `2a9ecb6` 双 reviewer 深度 review（Codex + general-purpose subagent）。
> Stage 1.7 / 1.8 / 2.4 是 review 后新增的 must-do 子任务（动手 Stage 4 前必须做完）。
> 本文档列**已知但暂不修**的问题，确保不被遗忘。

## Stage 4 第一周必处理（不阻塞动工但要尽早）

### C2 — embedding_dim 漂移无 startup guard

**症状**：`config.embedding.dimension` 改后，老 db workspace 的 collection 仍是旧 dim。Milvus collection schema 不可演进（plan §2 + §7），写入侧会得到 dim mismatch hard error。
**位置**：`crates/veda-server/src/state.rs:24-25` / `crates/veda-server/src/main.rs:75-83`
**决定（2026-05-28）**：不做 startup guard。Runtime 第一次 upsert 即得 Milvus `dim mismatch` hard error，运维（= Joe）看到错误能立刻识别为 config drift 自行处理。新增校验代码不抵这个简化的收益（参考 codex review + Joe 决策）。

### C4 — workspace 软删不级联 datasets

**症状**：`delete_workspace` 仅 `UPDATE veda_workspaces SET status='archived'`，`veda_datasets` 留 `active`。Stage 4 dataset list / 跨 workspace 查询会出现"workspace 不可达但 dataset 仍在"的不一致。
**位置**：`crates/veda-server/src/routes/account.rs:390-397` / `crates/veda-store/src/mysql.rs:2262-2267`
**决策点**（选一）：
- A. workspace archive 时级联 archive datasets
- B. dataset list 必须 join `workspace.status='active'`
**承诺**：Stage 4 实现 dataset CRUD 时决定。

### I1 — text 单位歧义（字符 vs 字节）

**决定（2026-05-28）**：Milvus VARCHAR `max_length` 实际单位是 UTF-8 字节（per 官方 operational FAQ），歧义不存在；同时把上限从 16384 提到 Milvus 硬上限 65535（对齐 Milvus 官方 BM25 tutorial）。代码 + 文档已对齐，记录 > 64 KiB 由 client chunk（Pinecone-style 契约）。

### I6 — operational visibility 单薄

**症状**：
- `provision_db_workspace` 成功路径无 `info!` 日志
- `create_vector_collection` 单个 index 失败时无结构化错误（只透传 Milvus message）
- `EmbeddingCache` 完全没 metrics（hit/miss 率、cache size、eviction rate）
**承诺**：Stage 4 上线前补关键 info! + 至少 2 个 metric（embed_cache_hits / embed_cache_misses）。

## 文档 drift（plan 回写）

低优先级，写代码时顺手回灌：

- `docs/archive/vectors-merge-plan.md` §1.1 SQL：`ENUM('fs','db')` → `VARCHAR(16) NOT NULL DEFAULT 'fs'`（Veda 风格）
- `docs/archive/vectors-merge-plan.md` §1.1 SQL：`expires_at TIMESTAMP` → `DATETIME`（2038 + timezone）
- `docs/archive/vectors-merge-plan.md` §4 EmbeddingCache：`try_get_with` → "manual batch-preserving moka lookup"（实际实现）
- `docs/archive/vectors-merge-plan.md` §10 dataset DELETE 行：标注"implemented in Stage 4"
- `docs/archive/vectors-merge-plan.md` Stage 1 表格："admin token endpoint 占位" 删除（已挪 Stage 4）
- `docs/archive/vectors-merge-plan.md` §6.1 structured collections 归属：补注"通过 AuthWorkspace fs-only 隐式 enforce"

## Subagent 提的 Minor（接受不修）

- `milvus.rs:454-456` "CollectionAlreadyExists" string match —— Milvus minor 版本可能改 message。v0 接受，failed-fast。
- text 字段恰好 65535 bytes UTF-8（含多字节字符）的 round-trip 集成测试 —— 边界覆盖空洞，codex review I1 时提的 follow-up，下一轮 hardening 顺手加
- `account.rs:319-326` default_dataset 用 `ws.created_at` 而非 `Utc::now()` —— 风格小问题，无功能影响。
- `embedding.rs:303` `Arc<Vec<f32>>` 但读侧仍 deep clone —— Arc 在当前 partition 模式收益小，但未来用 try_get_with 时省 clone。
- Workspace struct 没 `#[derive(Default)]` —— test fixture 多写几行而已。

## Stage 1.6 — HTTP auth kind 测试（Stage 1.5 review 时遗留）

**症状**：auth.rs `AuthWorkspace` kind=Fs 校验在 db workspace 上是否真返 400 workspace_kind_mismatch，目前只靠代码 review 信心，无 HTTP 层端到端测试。
**承诺**：Stage 5 E2E 测试覆盖（届时 vectors API 也存在，一起覆盖）。

## Stage 4.5 风险验证

**delete-by-pk filter 长度上限**（Codex Stage 4.3 review Q4）：500 个 PK × ~128 字节构造 `pk in [...]` filter 接近 65KB。Milvus 2.6 REST filter 字符串长度上限未确认。Stage 4.5 加一个 500-pk delete 测试，失败则下调 `MAX_PK_BATCH`。

**Filter DSL E2E 真实 Milvus 验证**（Codex Stage 4.4 review Q8）：filter.rs 有 15 单测验证字符串生成，但**没有真实 Milvus 验证表达式被接受**。Stage 4.5 加 E2E：upsert 含 meta 字段的多行 → 用 eq / range / in (OR-expansion) 搜索 → 验证 Milvus 2.6.14 接受 `meta["x"]` 语法。如果某 op 被 Milvus 拒绝，parser 改输出形态或砍掉该 op。

## Stage 4.5 验证清单（review 流转中累积）

集成测试时务必覆盖：
- **MySQL collation case-insensitivity**：默认 `utf8mb4_0900_ai_ci`，dataset "Foo" 和 "foo" 会撞 UNIQUE。Stage 4.1 review Q5。决定是否接受这个行为（推荐接受，"避免大小写歧义"是好事），文档化即可
- **URL-decoded {name} 路径含 reserved 字符**：%3A → ":" 是否被 validate_dataset_name 在 axum Path 提取后拦下
- **empty / oversized name** in POST body 和 DELETE path
- **duplicate create** 返 409（不是 500）
- **DELETE default** 在 auth 通过后返 400（不是 401/404）
- vectors upsert 路径的 dataset "Foo" vs "foo" 行为（如果 dataset 写入 Milvus 用 verbatim "Foo"，但 list/lookup 用 case-insensitive，会不一致）

## Stage 4 设计指引（Codex Stage 2.4 review Q4）

`VectorWorkspaceStore` trait 在 Stage 4 扩 upsert/search/query/delete 时，**方法签名收 Veda 领域 DTO（workspace_id / dataset / records / filter），不要让 routes 层传 collection_name + Milvus payload**。否则物理命名 + Milvus expr 语法泄露到 handler 层，后续要 breaking cleanup。

具体：
```rust
// 推荐
async fn upsert(&self, workspace_id: &str, dataset: &str, records: &[Record]) -> Result<UpsertResult>;

// 反例
async fn upsert(&self, collection_name: &str, milvus_payload: serde_json::Value) -> Result<...>;
```

impl 内部处理 `vector_collection_name` 拼接、`pk = "{dataset}:{row_key}"` 合成、Milvus expr 翻译。

## 已修（review 后立即处理，不在本 backlog）

- Stage 1.7：auth scope 缺口（allowed_workspaces / expires_at 未读）+ `AuthAccount::load_db_workspace` 强制 wrapper
- Stage 1.8：validator 模块（dataset/row_key/text/meta/tags + PK 长度校验，21 个单测）
- Stage 2.4：`VectorWorkspaceStore` trait 替代 AppState 双暴露 `milvus` 字段
