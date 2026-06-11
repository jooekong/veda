# Veda Module-by-Module Review & Evolution Plan

**Date:** 2026-04-29
**Reviewer:** Claude (deep-review)
**Scope:** 全 8 个 crate 的代码 bug、设计缺陷、未来演进计划
**Code version:** `1e33458` (P0/P1 fixes 已合入)
**Files analyzed:** ~30 主要源文件（不含 tests/ 与 mock）

## 本次执行状态（2026-04-29，Joe 会话）

> 说明：以下状态是本次会话的实际落地进度，用于下一 thread 续做。  
> 原评审条目保留不改，这里只记录“做了什么 / 还没做什么”。

### 已完成

- ✅ 1.1 删除 `From<String> for VedaError` 隐式转换（并修正对应测试）
- ✅ 1.2 `SearchHit.chunk_index` 改为 `Option<i32>`，移除 `-1` 哨兵（含 SQL schema 输出兼容）
- ✅ 2.5 `CollectionService::create` 在 Milvus 回滚失败时记录 error log
- ✅ 3.5 `insert_dentry` 的冲突识别改为精确 duplicate entry 路径（避免过宽误判）
- ✅ 3.6 过期 `processing` 任务会消耗重试次数（`claim()` 重认领递增；并补注释说明与 `fail()` 路径互斥）
- ✅ 3.12 Milvus client 请求增加重试（429/5xx/连接错误，指数退避）
- ✅ 4.2 Embedding retry 判定改为结构化状态判断，不再依赖字符串 contains
- ✅ 4.3 LLM provider 增加重试（429/5xx/连接错误，指数退避）
- ✅ 5.7 SQL 路由改为 Arrow `ArrayWriter` 直接输出 JSON，去掉 JSONL→String→parse 中转
- ✅ 6.6 删除恒 `ok` 的 `/v1/health`，保留 `/v1/ready`
- ✅ 7.5 CLI `reqwest` client 增加 connect/request timeout

### 未完成（留到下一 thread）

- ⏳ **Tier-A 大项**：Reconciler、Worker 原子化改造、ChunkSync 去重+同 file 串行、大文件链路流式化、Prometheus、MySQL pool 全参数化+migrate 拆分
- ⏳ **安全与租户能力**：ACL、Quota、Login 防爆、JWT kid/blacklist
- ⏳ **SQL/性能**：SessionContext 缓存、`veda_fs()` 流式执行、`has_pending_event` 索引化、`storage_stats` 增量统计
- ⏳ **Pipeline 能力**：token-aware truncation、tree-sitter chunking、PDF/OCR、citation 稳定化
- ⏳ **FUSE 线**：本次未处理（`veda-fuse` 不在默认 workspace members，需单独线程）

---

## 总评

**判定：尚未生产就绪（no-ship for production）。** 仅适合本地开发、受控 demo、无真实用户数据 / 无大文件 / 可接受数据漂移的内部环境。在下面 Tier-A 阻断项全部清理之前，不应承载任何对正确性或可用性有要求的工作负载。

P0/P1 production-readiness 修复已合入：env override、`/v1/ready`、JWT 长度校验、CORS 白名单、search limit 上限、binary write 不再 panic、embedding 切批+重试。这些把 server 从"会崩"提升到"能跑"，但**离生产标准还有两个量级的差距**——下面列的两条阻断项任何一条命中都会出真实事故：

1. **没有 reconciler，MySQL ↔ Milvus 漂移无法自愈**——worker 处理与 `complete()` 非原子（`worker.rs:93-95`）、Milvus upsert+delete 非原子（`milvus.rs:481-521`）、`enqueue_summary_sync` 在 `complete()` 之后（`worker.rs:96-99`）、ChunkSync 入队不去重 + 同 file_id 并发处理（详见 §场景 1）。任一环节出错就永久不一致，搜索召回率随时间下降；当前没有任何机制能恢复
2. **大文件路径全是"先 read 全量到内存再切片"**——`read_file_range`（`service/fs.rs:436`）、worker `handle_chunk_sync`（`worker.rs:151`）、FUSE `setattr` 截断（`fs.rs:373`）、FUSE `open(write)`（`fs.rs:484`）、CLI `Cp`（`main.rs:240`）。50MB 文件配多并发就足够把 server / FUSE / CLI 任意一端打挂
3. **多人协作场景下事件订阅不可信**——`veda_fs_events` 无 retention，cursor 失效无协议（详见 §场景 2）。client 长期离线后 cursor 比 server max_id 还大就永远收不到事件，且 server 不会告知

要把这份评审当作发布评审依据时，正确读法是：**Tier-A 全部清理 + 至少跑过一次 Reconciler 实战 + 大文件路径 e2e 压测通过，才能讨论 ship**。

下面按模块给出具体证据 + 演进计划。

---

## 1. veda-types

### 现状
零依赖的领域类型层，13 个单元测试，结构清晰。

### Bug / 缺陷

**1.1 `From<String> for VedaError` 自动映射成 `Internal`** — `errors.rs:76-80`
```rust
impl From<String> for VedaError {
    fn from(s: String) -> Self { VedaError::Internal(s) }
}
```
这是隐藏的语义陷阱：任何代码里 `?` 接到 String 都会变成"内部错误"，丢失了 `NotFound` / `InvalidInput` 等语义。在日志和告警里看到一片 `internal error` 而无法区分根因。建议删掉，强制调用方显式构造。

**1.2 `SearchHit.chunk_index: i32` 用 `-1` 当哨兵** — `milvus.rs:301`
```rust
SearchHit { ..., chunk_index: -1, ... }   // summary 命中时
```
sentinel 值是反 schema 设计。建议改成 `chunk_index: Option<i32>`，summary 命中显式 None。同样 `path: Option<String>`、`l0_abstract: Option<String>` 已经做对了。

**1.3 域类型与 wire 类型混用同一个 struct**
`Account.password_hash`、`ApiKeyRecord.key_hash`、`WorkspaceKey.key_hash` 都是 `pub` + `#[serde(skip_serializing)]`，靠 serde 防止外泄。一旦未来加一个新接口忘了加 `skip_serializing` 就漏出去。建议拆 `AccountInternal`（含 hash）与 `AccountPublic`（DTO），强类型隔离。

**1.4 缺少 `WorkspaceKey.account_id` / `created_by`** — 与 P1-6 关联
当前 workspace key 不知道是谁创建的，审计断了。建议直接在类型里加字段。

### 演进计划
- **P0** — 删除 `From<String> for VedaError`，逐步替换调用点
- **P1** — `SearchHit.chunk_index` 改 `Option<i32>`，删除 `-1` 哨兵
- **P1** — 增加 `AuditLog`、`Quota`、`Acl` 类型（先建 schema，与 server 同步落地）
- **P2** — 拆分 internal vs public DTO，避免 `skip_serializing` 误用
- **P2** — 引入 `serde(deny_unknown_fields)` 防止前向兼容意外

---

## 2. veda-core

### 现状
trait 定义 + FsService/SearchService/CollectionService。`cargo test -p veda-core` 当前 63 通过（workspace 全量 148 通过 + 22 ignored 集成测试）。retry_on_deadlock 实现得很到位。

### Bug / 缺陷

**2.1 `read_file_range` 先读全文再切片** — `service/fs.rs:436-447`
```rust
pub async fn read_file_range(...) -> Result<(Vec<u8>, u64)> {
    let content = self.read_file(workspace_id, raw_path).await?;
    let bytes = content.as_bytes();
    ...
}
```
chunked 文件 50MB 上限 → 任何 Range 请求都把 50MB 拉进内存。FUSE Range 读取（4MB chunk）也会触发同样行为。**P1-12 在 audit 中标了，未修。**

修法：chunked 路径直接按 byte 偏移定位 chunks，只读相关 chunks。

**2.2 `get_file_chunks_conn` SQL 在请求范围超出 EOF 时返回多余 chunk** — `mysql.rs:603-647`
```sql
chunk_index >= COALESCE((SELECT MAX(chunk_index) FROM ... WHERE start_line <= ?), 0)
AND start_line <= ?
```
设想 file 有 chunks `[(idx=0, start_line=1), (idx=1, start_line=100)]`：
- 请求 `start_line=200, end_line=300`（远超 EOF）
- 子查询 `MAX(chunk_index) WHERE start_line <= 200` → 1
- 外层 `chunk_index >= 1 AND start_line <= 300` → 仍然返回 idx=1 那个 chunk

请求的范围在文件之外却拿到了"最后一个 chunk"。当前被 `read_file_lines` 的 `if start > lc { return "" }` 提前 return 遮蔽（service/fs.rs:470-473），但这是上层补丁，下次复用 `get_file_chunks` 的人会踩。同样的，`(start_line=Some, end_line=None)` 路径也命中此问题。

修法：把 EOF 检查下沉到 SQL：在外层补 `start_line + line_count > ?`（即"chunk 末行 ≥ 请求 start"）。或者两段 SQL：先 `SELECT MAX(start_line)` 判定 EOF，再查范围。建表时已经有 `idx_line_lookup (file_id, start_line)`，加条件几乎零成本。

**2.3 `append_file_once` 异常路径不显式 rollback** — `service/fs.rs:855-901`
路径上有多个 `return Err(...)` 而 tx 只能靠 Drop 回滚。sqlx Drop 回滚是实现细节，不能靠它做正确性。建议每个 early-return 前显式 `tx.rollback().await.ok()`，与函数下半段的处理风格一致。

**2.4 SearchService `path_prefix` 过滤丢弃未解析路径的命中** — `service/search.rs:92`
```rust
hits.retain(|h| h.path.as_ref().map_or(false, |p| p.starts_with(prefix)));
```
当 Milvus 返回了 chunk 但 dentry 已被删除（典型的漂移场景），`path` 解析为 None，`map_or(false, ...)` 直接丢弃。本意是过滤 path_prefix，但副作用是默默吞掉漂移结果。在没有 reconciler 的当下，孤儿向量会持续累积，搜索召回率慢慢下降。

修法：先 `retain` 已解析的，未解析的单独打 metric `vector_orphan_total{workspace}`，便于 reconciler 触发。

**2.5 `CollectionService::create` 失败时静默丢弃 Milvus drop 错误** — `service/collection.rs:75-78`
```rust
if let Err(e) = self.meta.create_collection_schema(&cs).await {
    let _ = self.vector.drop_dynamic_collection(&milvus_name).await;
    return Err(e);
}
```
若 MySQL 写入失败 + Milvus 回滚也失败 → Milvus 集合泄漏，下次同名 create 报"already exists"。日志都没打。

**2.6 trait 默认实现是 N+1**
- `MetadataStore::get_files_batch` 默认逐个查 — MysqlStore 已 override
- `MetadataStore::get_dentry_paths_by_file_ids` 默认 N+1 — MysqlStore 已 override
- `MetadataTx::insert_fs_events` 默认逐条 — MysqlStore 已 override

风险：如果将来加新存储后端忘记 override，性能裸奔无人察觉。建议把这些方法标 `#[must_override = "performance critical"]` 或干脆去掉默认实现。

### 演进计划
- **P0** — `read_file_range` chunked-aware（按 byte 偏移定位 chunks）
- **P0** — Reconciler 设计：定期对账 MySQL ↔ Milvus，以 MySQL 为准
- **P1** — 修 `get_file_chunks_conn` 边界 SQL bug，加单测
- **P1** — Drop 一致的事务：所有 early-return 显式 rollback
- **P2** — Doc-level / row-level ACL 实现（架构已规划）
- **P2** — Quota service：在 FsService 入口检查 storage / count / api 限额
- **P3** — 把 trait 默认实现去掉或加性能标记

---

## 3. veda-store

### 现状
MySQL + Milvus 实现。schema migration 完整，outbox 模式实现细致（含 SKIP LOCKED 和 dead 状态机）。

### Bug / 缺陷

**3.1 `path_hash` 是 STORED 生成列，依赖 MySQL ≥ 5.7** — `mysql.rs:390`
```sql
path_hash VARCHAR(64) AS (SHA2(path, 256)) STORED
```
若部署到 MariaDB 或老版本 MySQL 会迁移失败。无 graceful fallback。

**3.2 `idx_ws_path_prefix (workspace_id, path(255))` 截断到 255 字节** — `mysql.rs:397`
但 `path VARCHAR(4096)` 允许 4096 字节。LIKE prefix 查询超过 255 字节时索引失效，全表扫描。在深路径多文件 workspace 中表现明显。

修法：要么把 `path` 长度统一收紧到 1024，要么改用 `path_hash` + 完整 `path` 比较。

**3.3 `migrate()` 启动无脑跑 + 多副本竞态** — `main.rs:43`
P2-4 audit 已标，未修。多副本同时启动会同时执行 `CREATE TABLE IF NOT EXISTS`（这部分 MySQL 安全），但加列等迁移操作就会冲突。

**3.4 sqlx pool 配置过窄**
`MysqlStore::with_max_connections` 只暴露 `max_connections`。没有：
- `min_connections`（冷启动慢）
- `acquire_timeout`（阻塞挂起）
- `idle_timeout`（连接滥用）
- `max_lifetime`（防止超长连接 stale）

生产环境必备。

**3.5 `MysqlMetadataTx::insert_dentry` 错误识别过宽** — `mysql.rs:1162`
```rust
Err(sqlx::Error::Database(ref db_err)) if db_err.code().as_deref() == Some("23000")
    => Err(VedaError::AlreadyExists(...))
```
SQLSTATE 23000 是所有完整性约束冲突的笼统状态。如果未来加外键约束，FK 冲突也会被误判成 "AlreadyExists"。改用 MySQL 的 errno 1062（duplicate entry）更精确。

**3.6 `claim()` 重入 retry_count 重复递增** — `mysql.rs:1599-1626`
当 lease 超时被重新 claim：
1. `retry_count + 1` 在 claim 时递增
2. process 失败后 `fail()` 又 `retry + 1`

一次故障被算两次。`max_retries` 默认 5，实际只能撑 ~3 次真实失败。

**3.7 `has_pending_event` 走 `JSON_EXTRACT` 全表过滤** — `mysql.rs:1715`
```sql
WHERE event_type = ? AND workspace_id = ? AND status IN ('pending','processing')
  AND JSON_UNQUOTE(JSON_EXTRACT(payload, ?)) = ?
```
JSON 函数无法用索引。每次 enqueue summary/dir-summary 都会调用一次 → outbox 积压时 enqueue 慢成一团。建议给 file_id / dentry_id 单独建一个 `payload_key` 列（generated index）。

**3.8 `storage_stats` 全表 COUNT** — `mysql.rs:941`
大 workspace 慢。建议增量物化 + cron 校准。

**3.9 Milvus `upsert_chunks` 顺序正确但缺乏可观测性** — `milvus.rs:481-521`
upsert → delete stale 顺序 OK，但中间崩溃会留下"不该存在的旧 chunk_index"。没有任何 metric 暴露这种漂移。

**3.10 Milvus 单集合多租户** — `milvus.rs:353-360`
P1-10 audit 已标。filter-based。备份/恢复无法按 tenant 粒度，ANN 索引召回质量受其他 tenant 数据污染。

**3.11 Milvus "hybrid_search" 实际不是真 hybrid** — `milvus.rs:382-418`
仅一个 search clause，RRF rerank 没东西可 rerank。本质等同于 ANN 加 LIKE filter 兜底。文档与命名误导。

**3.12 Milvus 客户端无重试** — `milvus.rs:95-130`
对比 EmbeddingProvider 有 3 次指数 backoff，Milvus 一次失败直接抛错。worker 任务因 Milvus 短暂抖动失败 → 整个 outbox 重试，浪费 LLM 调用。

### 演进计划
- **P0** — pool 全套参数化（min/idle/acquire/lifetime）
- **P0** — Milvus client 加 retry（429/5xx/connect）
- **P1** — Reconciler：周期对比 MySQL `veda_files`/`veda_summaries` 与 Milvus 集合，以 MySQL 为准修复 Milvus
- **P1** — schema migration 拆出 `veda-migrate` 独立 binary，server 启动只校验 schema 版本
- **P1** — outbox 增加 `payload_key VARCHAR(64)` generated column + index，`has_pending_event` 走索引
- **P1** — `storage_stats` 改增量统计（每次 fs op 更新计数表）
- **P2** — Milvus 多租户隔离：per-workspace partition 或 per-account collection
- **P2** — fulltext 真做：接 Meilisearch 或 Milvus 2.5+ 的 BM25
- **P2** — 1062 错码精确识别，去掉 23000 宽匹配

---

## 4. veda-pipeline

### 现状
embedding（含切批+重试，符合 P0-4 修复）、LLM、chunking、summary、extraction。

### Bug / 缺陷

**4.1 `EmbeddingProvider::embed` 串行执行批次** — `embedding.rs:230-235`
```rust
for batch in texts.chunks(BATCH_SIZE) {
    let batch_result = self.embed_with_retry(batch).await?;
    all.extend(batch_result);
}
```
5000 条文本 → 50 批串行，假设每批 200ms 就是 10 秒。简单的 `buffered_unordered` 并发就能拉成 ~200ms。批次失败仍然 fail-fast，整批前功尽弃。

**4.2 `is_retryable` 用字符串子串匹配** — `embedding.rs:203-217`
```rust
msg.contains("429") || msg.contains("500") || ...
```
任何错误响应正文里恰好含有 "500" 字样的都会触发重试（例如 422 错误描述里写到 "rate 500 r/s"）。建议把 EmbedError 携带 status code，按 code 判断。

**4.3 LlmProvider 无重试** — `llm.rs`
对比 embedding 有重试，LLM 没有。一次 503 → outbox SummarySync 重新跑（重新 LLM + 重新 embed）。建议同样加 3 次指数 backoff。

**4.4 LLM 没有 token 计数** — `summary.rs:54`, `llm.rs`
`truncate_content(content, 12_000)` 用字符数估算 → 中日韩文 token 数差很多。可能超模型上下文。建议接 tokenizer crate（tiktoken-rs / hf-tokenizers）做精确截断。

**4.5 chunking 仅识别 markdown 标题** — `chunking.rs:11-26`
对 Rust/Python/Go 源码、AsciiDoc、ReST 等没有 heading 概念的文件，所有内容塞进单一 section，再被 sliding window 切。质量差。建议：
- 对代码：按函数/类/段落（用 tree-sitter）
- 对 ReST/AsciiDoc：识别其 heading 语法
- 默认行为：基于段落分隔（空行）

**4.6 chunk index 不稳定** — P2-5 audit
重切后 `chunk_index` 全部变化，历史引用断。建议引用改成 `(file_id, content_sha256)` 或 `(file_id, start_line, end_line)`。

**4.7 PDF / OCR 完全没实现** — `extraction.rs:4-13`
仅 text/plain。`SourceType::Pdf` / `Image` 在 types 里有定义但流水线不支持。

### 演进计划
- **P0** — LlmProvider 加 retry（同 embedding 模式）
- **P1** — Embedding 批次并发执行（buffered_unordered，可配置并发度）
- **P1** — `is_retryable` 改用 status code，不用字符串匹配
- **P1** — Token-aware truncation（tiktoken-rs）
- **P2** — 代码语言感知 chunking（tree-sitter）
- **P2** — 稳定 citation：line range 或 chunk content hash
- **P2** — PDF 提取（pdf-extract / lopdf）
- **P3** — OCR（tesseract bindings 或外部 service）

---

## 5. veda-sql

### 现状
DataFusion 集成。`files` 表 + 8 个 fs UDF + 5 个 UDTF。filter pushdown 到 path prefix。`cargo test -p veda-sql` 当前 44 通过。

### Bug / 缺陷

**5.1 每个 SQL 请求都重建 SessionContext + 重新 list collection schemas** — `engine.rs:55-114`
对一个有 100 个 collection 的 workspace 来说，每次 SQL 都触发一次 `list_collection_schemas` + 100 次 register_table。Hot path 极慢。

修法：per-workspace SessionContext 缓存（弱键 LRU）+ schema 变更时 invalidate（通过 outbox event 通知）。

**5.2 `block_on` 双层风险** — `fs_udf.rs:95-103`
```rust
match handle.runtime_flavor() {
    MultiThread => task::block_in_place(|| handle.block_on(f)),
    _ => std::thread::scope(...)
}
```
`block_in_place` 把当前 worker 临时变阻塞，依赖 multi-thread runtime 有空闲 worker。如果 SQL 中包含很多 `veda_read` 调用，每行触发一次 block_in_place，runtime 的 worker 池被打满 → 整体卡死。

修法：对批量 UDF 请求改成 `tokio::runtime::Handle::block_on` 在专门的 IO 线程池运行；或者把 fs UDF 重写成原生 async DataFusion ScalarFunction（DF 0.43+ 支持）。

**5.3 `veda_write` UDF 不是事务化** — `fs_udf.rs:216-232`
`INSERT INTO files SELECT ... veda_write(path, content)` 写一半失败 → 部分 dentry 已写，部分没写，无法回滚。

修法：批模式收集所有 (path, content)，最后一次性提交；或干脆禁止在写 UDF 中使用大批量数据。

**5.4 `veda_fs()` 全量内存读取** — `fs_table.rs:155-191`
glob 读取 100MB 上限 + 单文件 50MB 上限，结合 `MemTable` 全部物化，峰值 100MB+ 内存。无 streaming。读 1GB 日志文件直接 OOM。

修法：实现 ExecutionPlan 流式输出（按文件 chunk 推进 RecordBatch）。

**5.5 `CollectionTable` schema 解析失败时静默** — `collection_table.rs:52`
```rust
serde_json::from_value(collection.schema_json.clone()).unwrap_or_default();
```
schema_json 损坏 → 返回空 fields，table 看起来是空的。应当 fail loud（让 register_table 直接报错）。

**5.6 `extract_path_prefix` 仅识别 `=` / `LIKE`** — `files_table.rs:80-130`
`path IN ('/a','/b')`、`path > '/x'`、`path BETWEEN ... AND ...` 都退化成全表扫描。

**5.7 sql.rs 路由把 RecordBatch 序列化为 JSON 再 parse 成 Value** — `routes/sql.rs:27-46`
冗余转换。`arrow::json::ArrayWriter` 可以直接给出 `Vec<Value>` 风格的 buffer。当前实现 CPU 浪费 + 内存峰值翻倍。

### 演进计划
- **P0** — `routes/sql.rs` 用 Arrow 直接序列化 JSON 数组，避免 String 中转
- **P1** — Per-workspace SessionContext 缓存（带 invalidate 信号）
- **P1** — `veda_fs()` 流式 ExecutionPlan
- **P1** — Filter pushdown 扩展：IN / 范围谓词
- **P2** — fs UDF 改成原生 async（DataFusion ≥ 0.43 的 AsyncScalarUDFImpl）
- **P2** — SQL 查询超时（DataFusion `with_query_timeout`）
- **P2** — SQL 内存上限（MemoryPool::FairSpillPool）
- **P3** — Cost-based 优化：SQL 包含 search() 时优先下推 limit

---

## 6. veda-server

### 现状
Axum 路由 + middleware + worker。env override / `/v1/ready` / search limit cap 已落地。

### Bug / 缺陷

**6.1 Worker 与 server 共进程** — `main.rs:99-120` (P1-8 audit)
单一二进制部署，多副本时 worker 重叠。需要拆成独立 binary `veda-worker`，K8s 用 Deployment 单独管控。

**6.2 worker 流程多处不原子**

a) `process_task` 与 `complete()` 非原子 — `worker.rs:93-95`，audit P0-2
   ```rust
   self.handle_chunk_sync(&task.workspace_id, file_id).await?;
   self.task_queue.complete(task.id).await?;
   ```
   中间崩溃 → 重 embed 一次。**没有 content_hash 跳过逻辑**（audit 推荐过）。

b) `enqueue_summary_sync` 在 `complete()` 之后 — `worker.rs:96-99`
   complete 成功但 enqueue 失败 → summary 永远不生成。应当在 ChunkSync 的同一个事务里写 outbox。

c) `handle_chunk_sync` 内 Milvus upsert+delete 非原子（已知，audit P0-2）

**6.3 worker 重读全文加载内存做 chunking** — `worker.rs:151-165`
chunked 文件 50MB 全部塞 String，再 `semantic_chunk`，再 `embed`。链路上每一步都要峰值 50MB。

修法：流式拉取 chunks，按 chunk 边界切 semantic chunk，分批 embed。

**6.4 `for_each_concurrent(batch_size, ...)` 没有全局并发上限** — `worker.rs:67-77`
batch_size 默认 10 → 最多 10 个 task 并发。每 task 内部 `EmbeddingProvider::embed` 是串行批次（`embedding.rs:230-235` for-loop），所以总 embedding HTTP 并发上限就是 10，不是早先草稿写的"~50+"。但仍然有两个真实问题：
- 多副本部署后 N×10 没有协调，embedding 服务限流就抖
- 单 task 没有 timeout，一个慢 LLM/embed 卡住一个并发槽位

修法：跨进程层面用 token bucket / Redis semaphore；单进程加 `tokio::time::timeout` 包住 process_task。

**6.5 没有 per-task 超时**
单个 task 的 LLM 调用配置 120s timeout。但 task 整体没有超时上限。一个文件 chunked 出 200 个 chunk，embedding 串行 → 可能数十分钟，期间 lease 已过期被另一 worker 重新 claim → 双跑。

**6.6 `/v1/health` 永远返回 ok** — `routes/mod.rs:36-38`
audit P0-3 提到。修复版 `/v1/ready` 已加，但 `/v1/health` 仍然永远 ok。两个端点语义重复且 `/health` 无用。

**6.7 JWT 不可撤销 / 无 kid** — `auth.rs:48-58` (P1-5)
HS256 单 secret，无黑名单。secret 一旦泄露所有 token 活到 exp。

**6.8 Login 无防爆** — `routes/account.rs:84-130` (P1-4)
无 IP rate limit，无失败计数，无 captcha。

**6.9 没有 Prometheus / metrics** — (P1-9)
operator 只能 SQL 看队列状态。无队列深度、失败率、embedding 延迟、连接池占用。

**6.10 没有 quota / rate limit** — (P1-2)
单 workspace、单 key 无限额。

**6.11 SQL 路由无 SQL 长度上限**
`POST /v1/sql` 接收任意长度 body（受 `DefaultBodyLimit` 全局默认 2MB 约束）。SQL parser 在恶意巨型查询下可能 OOM。

**6.12 `/v1/events` SSE 无 path_prefix 过滤** — `routes/events.rs:36-66`
全 workspace 事件推给 client，FUSE 自己过滤。带宽和 CPU 浪费。

**6.13 SSE 错误时只 backoff，不 yield error 给 client**
client 看不到任何反馈，以为流还活着。建议失败 N 次后 yield 一个错误 SSE event 然后断开。

### 演进计划
- **P0** — Worker 拆 `veda-worker` 独立 binary，部署解耦
- **P0** — 引入 `last_embedded_content_hash` 字段，worker 开始 ChunkSync 前比对，相同则跳过 embed
- **P0** — Prometheus `/metrics` 端点（axum-prometheus），暴露：outbox 深度/失败/dead、embed 延迟/费用、pool status、search QPS
- **P1** — Worker 单 task 超时上限（30 分钟），超时后主动 fail
- **P1** — Worker chunked 文件流式 embed（消除全文 String 拉取）
- **P1** — Login + per-IP rate limit + lockout（用 redis 或 in-memory token bucket）
- **P1** — JWT kid + 多 secret 同时有效；可选 Redis blacklist
- **P1** — Quota 表 + FsService 入口 enforcement
- **P1** — 删除 `/v1/health`（或定义为最便宜的存活探针），保留 `/v1/ready`
- **P2** — SSE path_prefix 过滤（`?path_prefix=/docs`）
- **P2** — `/v1/sql` 加 size + execution timeout
- **P2** — 审计日志（写入 `veda_audit_logs`）
- **P2** — OpenTelemetry trace propagation

---

## 7. veda-cli

### 现状
clap 命令行，纯 HTTP 客户端，config 持久化到 `~/.config/veda/config.toml`（mode 0600）。

### Bug / 缺陷

**7.1 `Cp` 把整个本地文件读到内存 + UTF-8 only** — `main.rs:240-248`
```rust
let content = std::fs::read_to_string(&src)?;
```
1GB 文件直接 OOM CLI。`read_to_string` 对非 UTF-8 走 `?` 返回错误（不是 panic），但仍然完全堵死了二进制上传路径。建议改成 `Vec<u8>` + streaming PUT（需要 server 端的 chunked 上传协议）。

**7.2 `Cat` 直接 print 远端内容** — `main.rs:253-254`
二进制文件会污染终端（控制字符 / ANSI 转义 / 编码错乱）。建议加 `--raw` flag 或检测到非 UTF-8 拒绝。

**7.3 `Account create / login --password=xxx` 走 shell argv**
密码进 shell history。建议改成 stdin 提示或 `--password-stdin`。

**7.4 `Client::check` 把 server 完整 body 当作错误信息抛出** — `client.rs:23-30`
错误响应里如果包含 SQL 片段、user input 等敏感内容，CLI 会原样打印。建议截断或脱敏。

**7.5 `reqwest::Client::new()` 没有 timeout** — `client.rs:18-19`
Server 卡死 → CLI 永远 hang。30s 默认是合理保护。

**7.6 没有 `veda summary` 拿目录摘要的明确 UX**
`Summary { path }` 只展示 L0/L1，但不显示 children — 用户难以判断是文件摘要还是目录摘要。

**7.7 `Search` 输出截断 80 char 没有 `--full` 选项**

### 演进计划
- **P1** — Streaming upload（chunked PUT 或 multipart）
- **P1** — `--password-stdin` 选项 + 交互式提示
- **P1** — `Cat` 二进制保护 + `--raw`
- **P1** — `Client` 加显式 timeout，错误信息截断
- **P2** — 进度条（indicatif crate）
- **P2** — `veda watch <path>` 走 SSE 监听文件变更（FUSE 之外的轻量替代）
- **P2** — 自动补全（clap_complete）
- **P3** — TUI 浏览器（ratatui）

---

## 8. veda-fuse

### 现状
fuser 0.14，子命令 mount/umount，daemon 化（fork+setsid），inode 双向映射 + nlookup，LRU read cache + generation 防竞态，SSE 失效，graceful unmount + watchdog。

### Bug / 缺陷

**8.1 `setattr` 截断 to non-zero 是非原子的"读-改-写"** — `fs.rs:373-388`
```rust
let mut bytes = self.client.read_file(&path)?;
bytes.truncate(target);  // or resize with zeros
self.client.write_file(&path, &bytes, rev)?;
```
中间被另一个 client 写入 → revision 失败 → ENOENT/EBUSY 给用户。文件大时还把全文拉本地。

**8.2 `open(O_WRONLY)` 不带 O_TRUNC 时拉全文进 WriteHandle.buf** — `fs.rs:484-489`
50MB 文件每次 open 都 50MB 内存。多个 client/process 同时 open 写就 N×50MB。

**8.3 `write` 稀疏 hole 会 zero-fill 整段** — `fs.rs:567-571`
```rust
if offset > handle.buf.len() { handle.buf.resize(offset, 0); }
```
进程 seek 到 1GB 写一字节 → 1GB 内存 → OOM。

**8.4 `flush_handle` clone 整个 buf** — `fs.rs:266-269`
50MB 写入 flush 时再翻一倍。建议 `std::mem::take(&mut h.buf)` 然后失败时 swap 回去。

**8.5 `read` cached 路径 size 上限算错** — `fs.rs:518-527`
```rust
let end = std::cmp::min(off + size as usize, cached.len());
```
若 `off + size` 溢出 usize 会 panic（非常大的 size）。建议 `off.saturating_add(size as usize)`。

**8.6 `make_attr` 用 `SystemTime::now()` 当所有时间戳** — `fs.rs:100-115`
每次 stat 返回不同 mtime → 任何依赖 mtime 的工具（make / ninja / rsync）都被打懵。Server 端 DTO `veda-types/src/api.rs:48-58` 已经有 `created_at` / `updated_at`（API 也返回），缺的只是 FUSE 客户端：`veda-fuse/src/client.rs:12-18` 的 `FileInfo` 没有这两个字段，`make_attr` 自然只能用 `now()`。

修法（一处改三步）：
1. `veda-fuse/src/client.rs` 的 `FileInfo` 加 `created_at: DateTime<Utc>` / `updated_at: DateTime<Utc>` 反序列化
2. `make_attr` 把 `info.updated_at` 转 `SystemTime` 写入 mtime/ctime，`created_at` 写 crtime
3. dir cache 路径 `attr_from_dir_entry` 同步加字段（DirEntry DTO 也要补）

**8.7 `inode::rename_path` 全表扫描所有路径** — `inode.rs:87-92`
```rust
self.path_to_ino.iter().filter(|(p, _)| p.starts_with(&prefix))
```
N=10 万 inode 时 rename 一个目录就遍历 10 万次。Trie 或排序结构能 O(log N + matched)。

**8.8 ReadCache `evict_lru` O(N)** — `cache.rs:107-118`
每次 eviction 扫所有 entries。1000 entry × 多次 eviction 可观开销。建议用 `LinkedHashMap` 或 `priority_queue`。

**8.9 SSE timeout 设成 None** — `sse.rs:46-48`
```rust
.timeout(None)
```
连接 dead 但 TCP 没收到 RST → 永远 hang。应该用 long-polling timeout（120s）+ 主动 reconnect。

**8.10 SSE 不区分 delete/move 事件** — `sse.rs:182-210`
都走 `invalidate_caches`，但 delete 应当 `inode_remove`，否则旧 path 一直存在 inode 表里。

**8.11 cursor save 在事件循环内同步 fs::write** — `sse.rs:110-114`
每秒一次小 IO 通常 OK，但 home dir 在网络盘上会卡。建议异步 channel 写。

**8.12 `client.rs` blocking client timeout 30s 对所有操作** — `client.rs:67-69`
50MB 文件 read 在慢网下 30s 不够。应当区分 connect/headers/body timeout。

**8.13 `read_file` 把整个 body 加载到 Vec<u8>** — `client.rs:127-129`
大文件 OOM。应当 stream 到磁盘临时文件。

**8.14 `setattr` 只处理 size，chmod/chown/utimes 静默忽略**
ls -l 看到的权限模式是常量 0o755/0o644，与用户预期不符。建议显式返回 ENOSYS 或 EPERM 让上层应用知晓。

**8.15 `statfs` 返回伪造的 1<<30 块数** — `fs.rs:733-735`
`df -h` 显示 1TB 容量。误导用户和监控。建议从 server 拉真实 `storage_stats`。

### 演进计划
- **P0** — `read` 路径加 saturating_add 防溢出（5 行）
- **P0** — `setattr` truncate 改用 server 端"按 size 截断" API（避免本地 read-modify-write）
- **P0** — `client::read_file` 流式落盘（mmap 临时文件，避免 OOM）
- **P1** — `WriteHandle.buf` 由 Vec<u8> 改成临时文件（spill to disk over 4MB）
- **P1** — `flush_handle` 用 `mem::take` + 失败回滚，避免 clone
- **P1** — Server FileInfo 扩展 created_at/updated_at，FUSE 用真实 mtime
- **P1** — SSE timeout(120s) + 心跳检测
- **P1** — SSE delete 事件走 `inode_remove`
- **P2** — Inode rename 改 trie 或前缀索引
- **P2** — ReadCache 用真正的 LRU 数据结构
- **P2** — `statfs` 拉真实 quota / storage_stats
- **P2** — 显式 `setattr` 返回 ENOSYS for unsupported ops
- **P3** — readahead / write-behind 优化

---

## 场景化分析

下面三个场景跨多个模块，单独看模块章节看不出全景。每节只讲新内容，重复点交叉引用回模块章节。

### 场景 1：频繁修改下的 chunk 漂移

#### MySQL 三条更新路径（事务内，原子）

| 路径 | 行为 | 来源 |
|---|---|---|
| 全量重写 `finalize_full_rewrite` | `delete_file_chunks(file_id)` 清空旧 chunks → `persist_write_meta` 重新插入 | `service/fs.rs:1148, 1198` |
| 增量 append | `delete_file_chunks_from(file_id, last_chunk_idx)` 删尾段 → 插入重切的 tail | `service/fs.rs:966-976` |
| COW（`ref_count > 1`） | 分配新 file_id，老 file_id 走孤儿清理 | `service/fs.rs:1156-1175` |

三条路径全在 `MetadataTx` 内，commit 失败整体回滚。**MySQL 侧不会留脏 chunks。**

#### Milvus 侧脏数据来源（按危害排序）

worker.rs:140-191 的 `handle_chunk_sync` 流程：消费 outbox → 重读全文 → semantic_chunk → embed → `upsert_chunks`（先 upsert 再 delete chunk_index > max）。

| # | 来源 | 后果 | 当前能否自愈 |
|---|---|---|---|
| 1 | **ChunkSync 缺去重**：`enqueue_summary_sync` 用了 `has_pending_event`（worker.rs:198-210），但写路径入队 ChunkSync 时**没有去重**。连续写同一文件 5 次 → 5 个 outbox event → 5 次 embed | embedding API 费用翻倍，最终一致性 OK | ⚠️ 不能消除浪费但不会脏 |
| 2 | Milvus upsert 成功 + delete 失败（详见 §3.9） | 留下 `chunk_index > new_max` 的孤儿 chunks，搜索召回旧内容 | ❌ 无 reconciler |
| 3 | worker `process_task` 与 `complete()` 之间崩溃（详见 §6.2a） | lease 过期重 claim → 重 embed 一次 | ❌ 无 `last_embedded_content_hash` 跳过 |
| 4 | **同 file_id 的 ChunkSync / ChunkDelete 并发处理**：outbox 按 id ASC，但 `for_each_concurrent(batch_size=10)` 让同 file_id 事件可能在不同 worker task 里并发 | delete 先于上一个 sync 完成 → 旧 chunks 短期内仍被召回 | ❌ 需要 per-file_id 串行 |
| 5 | ChunkDelete 处理失败 → outbox 重试，重试也失败到 dead | Milvus 永留孤儿向量 | ❌ 无 reconciler |

**结论**：MySQL 干净；Milvus 在频繁写场景下逐渐积累两类脏数据（孤儿 chunk_index、费用浪费），且**当前代码没有任何机制能消除**——这是 reconciler 必须补的缺口。新发现：来源 1 和 4 不在原 audit 里。

---

### 场景 2：离线/弱网下事件订阅可靠性

#### 当前能力清单

| 项 | 状态 | 备注 |
|---|---|---|
| 事件持久化 | ✅ | 每次 fs op 在业务事务内插入 `veda_fs_events`（service/fs.rs:362, 685, 818, 992） |
| FUSE cursor 落盘 | ✅ | atomic rename 写法（fuse/sse.rs:172-180），进程重启可继续 |
| 重连追赶 | ✅ | `?since_id={cursor}` 拿到错过的事件 |
| `veda_fs_events` retention | ❌ | 无 TTL/cleanup，表无限增长 |
| cursor 失效协议 | ❌ | cursor > max(id) 时 server 静默返回空，client 永远不知道事件被清过 |
| bootstrap 协议 | ❌ | 新 client `since_id=0` 拿到的是变更流不是状态快照，必须额外 stat/list_dir |
| server-side path_prefix 过滤 | ❌ | 详见 §6.12，整 workspace 事件无差别广播 |
| 心跳 / cursor watermark | ⚠️ | TCP keepalive 有（events.rs:68），但静默期 cursor 不推进 |
| poll 间隔 | ⚠️ | 1s server poll + 1s SSE poll → ≤2s 看到他人修改 |

#### 要让"离线/弱网完全可信"还缺什么

1. **事件保留窗口 + 410 Gone 协议**：例如保留 7 天，client cursor 落在窗口外 server 返 410，client 必须 full resync
2. **server-side path_prefix 过滤**：`/v1/events?path_prefix=/docs` 由 server 应用 LIKE，不是把全量事件推给客户端再过滤
3. **可选：watermark 心跳**：每 N 秒发 `{type:"heartbeat", id:max_id}`，让 cursor 在静默期推进
4. **可选：long-lag 自动 full resync**：FUSE 在 cursor 落后 server max_id 太多时主动 invalidate 全部 cache + 重新 list_dir

---

### 场景 3：细粒度权限（ACL）路线图

#### 当前模型（一句话）

只有 **workspace 级 + binary 读写**：workspace key 持有 `KeyPermission::{Read, ReadWrite}`（types.rs:29-32），写路由前置 `require_write()`（auth.rs:115-120）。**同一 workspace 内所有 ReadWrite key 看到的文件完全相同，无任何 path/file 级隔离。**

#### Minimal schema（前缀继承）

```sql
CREATE TABLE veda_acls (
    id VARCHAR(36) PRIMARY KEY,
    workspace_id VARCHAR(36) NOT NULL,
    subject_type ENUM('account','group','key') NOT NULL,
    subject_id VARCHAR(36) NOT NULL,
    path_prefix VARCHAR(4096) NOT NULL,
    permission ENUM('none','read','readwrite') NOT NULL,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    INDEX idx_ws_subject (workspace_id, subject_id),
    INDEX idx_prefix (workspace_id, path_prefix(255))
);
```

**前缀继承**比 glob 简单：评估时取**最长匹配前缀**作为有效权限，`/docs` set readwrite + `/docs/secret` set none 自动生效。

#### Evaluate 算法（FsService 入口）

```rust
let perm = acl.evaluate(workspace_id, account_id, &normalized_path).await?;
match (op, perm) {
    (Op::Read, None)             => return Err(NotFound),       // 防探测
    (Op::Write, Read | None)     => return Err(PermissionDenied),
    _ => {}
}
```

关键决策：**read 拒绝返 NotFound 而非 Forbidden**，类 GitHub 私有 repo 行为，避免侧信道泄漏目录结构。

#### 搜索过滤（最难的部分，三选一）

| 方案 | 做法 | Tradeoff |
|---|---|---|
| **A. 后过滤** | ANN 拿 3*limit，应用层按 ACL 过滤 → truncate | 简单；ACL 覆盖小时召回率被打到 0 |
| **B. Milvus filter 注入** | 拉用户的 readable path_prefix，编进 filter `path like "/docs/%" or ...` | 中等；需要 Milvus chunk schema 加 `path` 字段（当前只有 file_id） |
| **C. ACL group 标签** | chunk 加 `acl_group_ids: ARRAY<INT>`，filter 用 `array_contains_any` | 性能最好；ACL 变更需 reindex chunks |

**实务推荐**：先 A 应付 ≤10 人 workspace，分桶部署后切 B。C 留给真正大规模多租户场景。

#### 工作量估计与前置依赖

| 项 | 估时 |
|---|---|
| ACL 表 + evaluate 算法 | 3 天 |
| FsService 入口接入 | 2 天 |
| SearchService 后过滤（方案 A） | 1 天 |
| veda_sql FilesTable 接入（否则 SQL 绕过 ACL） | 1 天 |
| 管理 API + 集成测试 | 2 天 |
| **小计** | **约 1.5 周（与 audit Tier-B-1 估算一致）** |

**强前置**：必须先修 P1-6 的 `account_id` 缺陷——目前 workspace-key 路径下 `account_id` 取自 workspace owner（auth.rs:191-193），如果 workspace 由 A 创建给 B 使用，ACL 评估时 subject 是错的。**ACL 必须基于真实操作人。**

---

## 跨模块的关键设计债（按修复优先级）

### Tier-A：上线前必须修
1. **Reconciler**（横跨 veda-store + veda-server worker）
2. **Worker 流程原子化**：last_embedded_content_hash 跳过 + outbox 与业务变更同事务
3. **ChunkSync 入队去重 + 同 file_id 串行化**（详见 §场景 1 来源 1/4）
4. **大文件链路改流式**：read_file_range / FUSE setattr / FUSE open(write) / worker chunk_sync 全都不再"先全量拉到 String"
5. **Prometheus metrics 全套**
6. **MySQL pool 全套参数化 + migration 拆 binary**
7. **LLM provider 加 retry**（与 embedding 对齐）

### Tier-B：partner 接入前补完
8. **Doc-level ACL**（详见 §场景 3，前置：先修 P1-6 account_id）
9. **Quota + rate limit**（types + auth middleware + store）
10. **Login 防爆 + JWT kid/blacklist**
11. **SQL 引擎 SessionContext 缓存**（性能）
12. **事件保留窗口 + cursor 失效协议 + SSE path_prefix 过滤**（详见 §场景 2）
13. **Worker 拆独立 binary**

### Tier-C：规模化产品化
14. **Milvus 多租户隔离**（partition）+ 真 hybrid search（BM25）
15. **Citation 稳定化**（line range 或 content hash）
16. **Connector / Import API**（S3/Git/Confluence）
17. **OpenAPI + SDK 生成**
18. **PDF / OCR 流水线**
19. **审计日志 + OpenTelemetry**
20. **代码语言感知 chunking**（tree-sitter）
21. **CollectionSync 实现**（worker 现在是 stub）

---

## 值得保留的好设计

- `path_hash` STORED column 做唯一约束 + 4096 长 path —— 思路正确，只是 `path(255)` prefix 索引一致性需要修
- Outbox `SKIP LOCKED` + lease + retry/dead 状态机 —— 写得相当扎实
- `retry_on_deadlock` helper —— 简洁有效
- ReadCache `generation` 防 invalidate→put 竞态 —— 巧妙
- FUSE daemon 化 + fork+pipe 通知父进程 + signal handler + 5s watchdog —— 工程化到位
- env override 实现 + 单测覆盖（包括 trailing comma、empty 等边界）
- `compute_write_meta` 单遍走 bytes 同时算 full sha + per-chunk sha + line count —— 性能与正确性都顾到
- API key 总是 hash 存储，明文只在 create response 出现一次 —— 对的安全姿势

---

## 文档差距

- `ARCHITECTURE.md:52-54` 已经把 Reconciler 列在"待实现"，但"已实现"那段对漂移风险只字未提。建议在 Reconciler 旁边加一行注脚说明"未实现期间 outbox 失败 = MySQL/Milvus 不一致"，让读者明白这不是可以延后的小项
- `docs/production-audit.md` 是 2026-04-28 的快照，与最新 commit `1e33458` 不一致（很多 P0/P1 已修但文档没回填）。建议 audit 加一栏 "fixed in commit"
- 没有 changelog 文件
- 没有迁移脚本说明（`migrate()` 内嵌在代码里，没有 SQL 文件）

---

## 附：审计方法

- 全量阅读 8 个 crate 的所有源文件（不含 tests/mock）
- 对照 `ARCHITECTURE.md` / `docs/production-audit.md` 验证现状
- 重点核查：并发原子性、内存峰值、错误传播、SQL 索引使用、HTTP 层超时、生产可观测性
- 不修改任何源代码与文档
