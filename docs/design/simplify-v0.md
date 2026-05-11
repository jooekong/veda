# Veda v0 简化方案

> 目标：收缩产品边界，砍掉当前收益不够的复杂度，让 alpha 能稳定跑起来。
> 原则：每项改动要么**降低维护风险**（改一处漏一处），要么**删掉不该在 v0 存在的代码路径**。

---

## 0. 修复文档 / 入口漂移

现状：AGENTS.md 指向不存在的路径，README 描述不存在的能力，ARCHITECTURE.md 自相矛盾。

| 问题 | 位置 | 修复 |
|------|------|------|
| `docs/PLANS.md` 不存在 | `AGENTS.md:13` | 改为 `docs/design/plans.md` |
| `docs/design.md` 不存在 | `AGENTS.md:14` | 改为 `docs/design/design.md` |
| README 写 `text/PDF/images` | `README.md:9` | 改为 `text files`，PDF/OCR 标为 planned |
| README 写 `REST API / WebSocket` | `README.md:18` | 改为 `REST API / SSE` |
| README 写 `veda cp ./report.pdf` | `README.md:123` | 删掉 PDF 示例 |
| README 写 `PDF/OCR extraction` | `README.md:186` | 改为 `text extraction (PDF/OCR planned)` |
| ARCHITECTURE 说 Reconciler 已有细节又写"待实现" | `ARCHITECTURE.md:45,61` | 统一：已实现的写已实现，未完成的只写待实现 |
| design.md 写 `PDF/图片` + `WebSocket` | `docs/design/design.md:14,18` | 对齐实际：text + SSE |

**工作量**：~30min，纯文档编辑。先做这一步，让所有文档与代码现状一致。

---

## 1. 收缩产品边界

v0 的产品定义：**文本文件 + 语义/全文搜索 + HTTP/CLI**。

以下能力标为实验或删掉入口：

| 能力 | 当前状态 | v0 处理 |
|------|----------|---------|
| PDF text extraction | `veda-pipeline/extraction.rs` 仅 14 行 `text/plain` 占位 | 删掉 README/design 里的 PDF 描述，保留占位代码不动 |
| Image OCR | 未实现 | 从 ARCHITECTURE 待实现列表中移到 future 区 |
| WebSocket | 未实现，实际用 SSE | 全文替换为 SSE |
| Raw Vector Collections | CLI 有示例但使用场景不清 | README 中移到 "Advanced" 小节 |
| Structured Collections | 已实现 | 保留，属于 v0 核心能力 |

---

## 2. 砍掉文件存储的过度设计

**现状**：`fs.rs` 2000+ 行，同时支持增量 append、COW/ref_count、chunk hash 水印。append 仍会加载所有 chunk 来算 checksum（`fs.rs:1121`），在 50MB 上限下收益为零。

**砍掉**：

| 删除项 | 涉及代码 | 理由 |
|--------|----------|------|
| 增量 append 路径 | `fs.rs` `append_file_once`、store 的 `get_last_file_chunk` | 全量重写 + revision CAS 语义更简单且正确 |
| COW / ref_count > 1 分支 | `fs.rs` `finalize_full_rewrite` 双分支、`copy_file` 的 ref_count 逻辑 | 50MB 上限下 copy-on-write 省不了多少，但让 write/delete/copy 各多一条路径 |
| `cleanup_file_if_orphan` 与内联 orphan 清理的二元性 | `fs.rs:859-867` vs `fs.rs:1280-1294` | 统一成一个路径，delete/copy 都走它 |

**保留**：revision CAS（`If-Match`/`If-None-Match`）、SHA256 dedup、分层存储（inline vs chunked）。

**简化后模型**：
- `write` = 全量写入，checksum dedup，revision 递增
- `copy` = 新建 dentry 指向同一 file_id（ref_count 保留但只做"是否可删 content"判断，不做 COW）
- `delete` = 删 dentry，ref_count 降到 0 则删 content
- `append` = 删掉，CLI/HTTP 层 append 改为 read + concat + write（调用方实现）

**预估影响**：`fs.rs` 减少约 300-400 行，`store.rs` 减少 `get_last_file_chunk`，mysql.rs 减少对应查询，mock 测试同步简化。

---

## 3. 拆 MetadataStore 万能 trait

**现状**：`store.rs:7` 的 `MetadataStore` 包含 dentry、file、chunk、outbox、summary、events、stats 全部方法，事务 trait `MetadataTx`（`store.rs:220`）又复制了大部分。`list_dentries_under_capped` 逐字重复 30 行。

**拆法**（按文件，不是按 crate）：

```
store.rs          →  store/mod.rs          (re-exports + MetadataStore 组合 trait)
                     store/dentry.rs       (DentryRepo)
                     store/file.rs         (FileRepo + ChunkRepo)
                     store/outbox.rs       (OutboxRepo + TaskQueue)
                     store/summary.rs      (SummaryRepo)
                     store/fs_event.rs     (FsEventRepo)
                     store/auth.rs         (AuthStore — 已独立)
                     store/collection.rs   (CollectionMetaStore — 已独立)
```

`MetadataStore` 变成组合 trait：

```rust
pub trait MetadataStore:
    DentryRepo + FileRepo + OutboxRepo + SummaryRepo + FsEventRepo + Send + Sync
{
    async fn ping(&self) -> Result<()>;
    async fn begin_tx(&self) -> Result<Box<dyn MetadataTx>>;
}
```

`list_dentries_under_capped` 作为 `DentryRepo` 的 provided method，`MetadataStore` 和 `MetadataTx` 共用同一份代码（因为都 impl `DentryRepo`）。

**附带收益**：
- `get_files_batch`、`get_dentry_paths_by_file_ids` 等 N+1 默认实现的"必须 override"约束更容易通过 trait 文档控制
- mock 可按 repo 组合，不用每次实现全部 30+ 方法

---

## 4. 拆 mysql.rs

**现状**：2354 行，schema bootstrap + row mapper + 全部 repo 实现 + 事务 + task queue + auth + collection meta。

**拆法**（与 trait 拆分对齐）：

```
mysql.rs  →  mysql/mod.rs          (MysqlStore struct + pool + migrate + ping)
             mysql/schema.rs       (CREATE TABLE IF NOT EXISTS 全部 DDL)
             mysql/rows.rs         (row_to_dentry, row_to_file, parse_* 等 mapper)
             mysql/dentry.rs       (impl DentryRepo for MysqlStore / MysqlMetadataTx)
             mysql/file.rs         (impl FileRepo)
             mysql/outbox.rs       (impl OutboxRepo + TaskQueue)
             mysql/summary.rs      (impl SummaryRepo)
             mysql/fs_event.rs     (impl FsEventRepo)
             mysql/auth.rs         (impl AuthStore)
             mysql/collection.rs   (impl CollectionMetaStore)
             mysql/tx.rs           (MysqlMetadataTx struct + commit/rollback)
```

同时解决的代码问题：

| 问题 | 当前位置 | 修复 |
|------|----------|------|
| SELECT 列清单 5+ 处手写重复 | `mysql.rs:625,667,682,883,1414` | `rows.rs` 定义 `const DENTRY_COLS`, `const FILE_COLS` |
| `IN (?,?,...)` 拼装重复 3 次 | `mysql.rs:921,1045,1245` | `rows.rs` 提供 `fn in_placeholders(n) -> String` |
| 死锁检测靠字符串 `"1213"` | `mysql.rs:21-24` | `mod.rs` 用 `sqlx::Error::Database` + `.number()` |
| `query_fs_events` 中 `Some("/")` 与 `None` SQL 相同 | `mysql.rs:1079-1124` | 合并为一个 arm |
| `path_hash` 建了但查询仍按 `path` | `mysql.rs:625` | 要么查询改用 `(workspace_id, path_hash, path)`，要么删掉 `path_hash` 列 |

---

## 5. Outbox 去重改用结构化字段

**现状**：dedupe 用 `JSON_EXTRACT(payload, '$.file_id')` 扫表（`mysql.rs:1827,2040`）。随着 outbox 量增长，这是性能隐患且无法走索引。

**改法**：

```sql
ALTER TABLE veda_outbox ADD COLUMN dedupe_key VARCHAR(255) DEFAULT NULL;
CREATE INDEX idx_outbox_dedupe ON veda_outbox(workspace_id, event_type, dedupe_key);
```

- `ChunkSync` → `dedupe_key = file_id`
- `SummarySync` → `dedupe_key = file_id`
- `DirSummarySync` → `dedupe_key = dentry_id`

去重查询变为：

```sql
SELECT id FROM veda_outbox
WHERE workspace_id = ? AND event_type = ? AND dedupe_key = ? AND status IN ('pending','running')
LIMIT 1
```

同时将 worker/reconciler 中分散的 `OutboxEvent` 构造集中到一个 `OutboxScheduler`（可放在 `veda-core` 或 `veda-server`），统一 debounce / burst / available_at 计算。

---

## 6. 删除 CollectionSync stub

**现状**：`OutboxEventType::CollectionSync` 存在于类型层，worker 直接 complete 并返回 "not implemented"（`worker.rs:200`）。

**改法**：
- 删除 `OutboxEventType::CollectionSync` variant
- 删除 worker 中对应的 match arm
- 删除 `veda-types` 中 serde 测试里的 CollectionSync case
- 等 collection schema reconciler 真要做时再加回来

**理由**：保留 stub 让系统能力边界变假——如果有人不小心入队了 CollectionSync 事件，worker 会 complete 它但什么都没做。

---

## 7. SQL 引擎改为只读

**现状**：`fs_udf.rs:44` 同时注册 `veda_write`/`veda_append`/`veda_remove`/`veda_mkdir`，用 sync UDF 桥 async（`fs_udf.rs:95`）。文件写入已有 HTTP/CLI 两条正路。

**砍掉**：
- 删除 `veda_write`、`veda_append`、`veda_remove`、`veda_mkdir` 四个写入 UDF
- `FsScalarUdf` 的 `read_only` 字段和 `check_write_allowed` 方法可一并删除
- `register_all` 从 9 个 UDF 降到 5 个（`veda_read`/`veda_exists`/`veda_size`/`veda_mtime` + `embedding`）

**保留**：`veda_read`（读文件内容到 SQL 结果集）、`veda_exists`/`veda_size`/`veda_mtime`（元数据查询）、`embedding()`（文本转向量）、`search()` UDTF、`veda_fs()` UDTF、`veda_fs_events()`、`veda_storage_stats()`。

**附带简化**：`FsScalarUdf` 的大 `match`（`fs_udf.rs:203-315`）减少 4 个分支，剩余分支可考虑改为 `enum FsOp` 分发。

---

## 8. FUSE 降级为 read-only v0

**现状**：`fs.rs:70` 同时管 inode、attr cache、read cache、dir cache、write handles、notify fd。截断走 full read-modify-write（`fs.rs:391`），SSE watcher 又重。

**改法**：
- `MountOpts` 中 `read_only` 默认改为 `true`
- 删除或 `#[cfg(feature = "fuse-write")]` 门控写入相关代码：`write`/`flush`/`create`/`mkdir`/`unlink`/`rmdir`/`rename`/`setattr(truncate)`
- 保留 `lookup`/`getattr`/`readdir`/`readdirplus`/`open`/`read`/`release`/`statfs`/`destroy`
- SSE watcher 保留（read-only 模式下用于 cache invalidation）

**收益**：`fs.rs` 从 ~850 行降到 ~500 行，不需要维护 `WriteHandle`/dirty 标记/flush+release 双写/多 fh 同 ino 写告警等复杂状态。等 HTTP 存储模型稳定后再放开写入。

---

## 9. 搜索 summary 命中的语义修正

**现状**：Milvus summary hit 把 `summary_id` 放进 `SearchHit.file_id`（`milvus.rs:385`），但 overview hydrate 按 `file_id` 查 summary（`search.rs:111`）。目录摘要命中时 `file_id` 实际存的是 `dentry_id`，语义混乱。

**改法**：

在 `veda-types` 引入：

```rust
pub enum SearchEntity {
    File(String),    // file_id
    Dir(String),     // dentry_id
}
```

`SearchHit` 改为：

```rust
pub struct SearchHit {
    pub entity: SearchEntity,
    pub path: Option<String>,
    pub content: String,
    pub score: f32,
    pub score_type: String,
    pub chunk_index: Option<i32>,
}
```

Milvus store 写入 summary 时用 `summary_type` 字段区分 file/dir，搜索返回时构造正确的 `SearchEntity`。`resolve_paths` 按 entity 类型走不同查询路径。

**影响范围**：`veda-types`（SearchHit）、`veda-store/milvus`（summary 写入/查询）、`veda-core/search`（resolve_paths）、`veda-server/routes/search`（DTO 映射）、`veda-cli`（显示）。属于跨 crate 改动，但语义收益大。

---

## 10. CLI 拆模块 + 复用 typed DTO

**现状**：`main.rs` 900+ 行单文件 match，JSON `data` 数组遍历重复 6+ 次。FUSE 有独立 blocking client。

**拆法**：

```
main.rs       →  main.rs              (Cli::parse + match 一层转发)
                 commands/account.rs   (create/login)
                 commands/workspace.rs (create/list/use)
                 commands/fs.rs        (cp/cat/ls/mv/rm/mkdir/append/grep)
                 commands/search.rs    (search/abstract/overview)
                 commands/collection.rs
                 commands/sql.rs
                 commands/init.rs      (已有 init.rs，迁入)
                 commands/status.rs    (已有 status.rs，迁入)
```

同时：
- CLI 的 `client.rs` 响应解析改用 `veda-types` 的 `ApiResponse<T>` 反序列化，不手写 `resp["data"]`
- 提取 `fn iter_data<T: DeserializeOwned>(resp) -> Result<Vec<T>>`
- `print_summary_layer` 改为返回 `Result<ExitCode>`，不直接 `process::exit`

**FUSE client 不动**：blocking vs async 栈不同，强行共享 crate 成本高于收益。但 SHA256 计算可提到 `veda-types` 作为 util。

---

## 附录：代码级修复（随上述重构顺带完成）

这些不单独列为工作项，在对应模块重构时一并处理：

| 问题 | 位置 | 随哪项修复 |
|------|------|-----------|
| `pipeline` embedding/llm 重试循环重复 | `embedding.rs`/`llm.rs` 各一套 | 独立小项，~30min |
| `veda-sql` `block_on` 散落在 `fs_udf` 被其他模块引用 | `embedding_udf.rs:11` | 随 #7 SQL 只读化 |
| `veda-sql` `FilesExec`/`CollectionExec` ExecutionPlan 骨架重复 | `files_table.rs`/`collection_table.rs` | 独立小项，~30min |
| `veda-server` `get_abstract`/`get_overview` 重复 | `routes/search.rs:47-86` | 随 #9 搜索语义修正 |
| `veda-server` bearer token 提取重复 | `auth.rs:74-88` vs `144-158` | 独立小项，~15min |
| FUSE `readdir`/`readdirplus` 大段重复 | `fs.rs:431-497` | 随 #8 FUSE read-only |
| `ensure_parents` async 递归 | `fs.rs:1422-1455` | 随 #2 fs 简化 |
| `CreateAccountResponse` ≡ `LoginResponse` | `api.rs:15-31` | 随 #10 CLI DTO 复用 |

---

## 执行顺序建议

按依赖关系和风险排序：

```
Phase A — 文档 + 删除死路径（0 风险，立即做）
  #0  修复文档漂移
  #1  收缩产品边界描述
  #6  删除 CollectionSync stub

Phase B — 减法重构（删代码，风险低）
  #7  SQL 引擎改只读
  #8  FUSE 降级 read-only
  #2  砍文件存储过度设计（append/COW）

Phase C — 结构重构（拆文件，风险中等）
  #3  拆 MetadataStore trait
  #4  拆 mysql.rs
  #10 CLI 拆模块

Phase D — 语义修正（跨 crate，风险较高）
  #5  Outbox dedupe_key
  #9  SearchEntity 语义修正
```

每个 Phase 做完可以出一个可工作的版本，不依赖后续 Phase。
