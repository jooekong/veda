# Code Review — Veda 全项目逐行审查

> 审查人：AI (top-tier programmer perspective)
> 日期：2026-04-16
> 范围：workspace 下所有 crate 的全部 `.rs` 源文件

---

## 一、Bug（确定性缺陷）

### 1. `read_file_lines` 先加载全量内容再校验参数

**文件**: `veda-core/src/service/fs.rs:207-230`

`start < 1 || end < start` 的校验发生在内容已从 DB 加载之后。应在做任何 I/O 之前先验证。

此外，chunked 模式下传入 `get_file_chunks(file_id, None, Some(end))` 未传 `start_line`，导致会把第一个 chunk 到 `end` 的所有 chunk 都加载进来。应该传 `Some(start)` 来裁剪。

### 2. MockTx 不具备事务语义，核心测试存在盲区

**文件**: `veda-core/tests/mock_store.rs`

`MockTx` 的每个写操作直接修改共享 `Arc<Mutex<MockState>>`，`commit` 和 `rollback` 都是空操作。这意味着：
- 所有写操作立即对外可见，不等 commit
- rollback 不会回滚任何数据

因此，所有依赖事务原子性的测试（如 `copy_file` 中 rollback 后旧数据应恢复）实际上并未被验证。

### 3. path 中的 `%` 和 `_` 导致 LIKE 查询匹配过宽

**文件**: `veda-store/src/mysql.rs:739, 766`

`list_dentries_under` 和 `delete_dentries_under` 用 `format!("{path_prefix}/%")` 构造 LIKE 模式，但未转义路径中可能存在的 `%` 和 `_`。`path::validate_segment` 只拒绝 `\0` 和 `:`，不拒绝 `%` 和 `_`。

如果一个目录叫 `/data_100%/`，LIKE 模式 `/data_100%/%` 会错误匹配 `/dataX100Y/anything`。

**修复**: 在构造 LIKE 模式前转义 `%` → `\%`、`_` → `\_`。

### 4. `veda_dentries` 唯一索引只有 255 字节前缀

**文件**: `veda-store/src/mysql.rs:342`

```sql
UNIQUE INDEX idx_ws_path (workspace_id, path(255))
```

`path` 列是 `VARCHAR(4096)`，但唯一性只在前 255 字节上强制。两个前 255 字节相同但后续不同的路径会冲突。应改为全长索引，或者将 path 列缩到 255 以内，或者用 hash 列做唯一约束。

### 5. `mkdir` 存在 check-then-act 竞态

**文件**: `veda-core/src/service/fs.rs:352-386`

`mkdir` 先在事务外用 `get_dentry` 检查是否存在，然后才开事务。并发请求可能同时通过检查，导致两个事务都尝试 insert 同一个 dentry。数据库唯一索引会报 `Storage` 错误，而不是优雅地返回幂等成功。

**修复**: 把 `get_dentry` 检查放到事务内，或者 catch 唯一键冲突错误并返回 Ok。

### 6. `search` 的 `path_prefix` 过滤在 vector 返回之后进行

**文件**: `veda-core/src/service/search.rs:89-93`

三种搜索模式（semantic / fulltext / hybrid）都不在 Milvus 侧过滤路径前缀，而是返回 `limit` 条结果后在 Rust 侧用 `retain` 过滤。这导致实际返回的结果数可能远小于 `limit`。

### 7. `login` 每次调用都创建新 API Key，无上限

**文件**: `veda-server/src/routes/account.rs:100-111`

每次 login 生成一个新的 `ApiKeyRecord`，不清理旧 key。频繁登录会在 `veda_api_keys` 表中积累无限行。

### 8. `read_file` 路由的 `list` 参数做了冗余映射

**文件**: `veda-server/src/routes/fs.rs:62-71`

`FsService::list_dir` 已经返回 `Vec<api::DirEntry>`，路由处理函数又把每个 `DirEntry` 的字段重新拷贝了一遍组装成同样的 `DirEntry`。直接使用即可。

---

## 二、潜在问题（需要关注但不一定立刻 crash）

### 9. `write_file` 硬编码 `mime_type: "text/plain"`

**文件**: `veda-core/src/service/fs.rs:98`

所有文件都存成 `text/plain`，`FileRecord` 上的 `mime_type` 和 `source_type` 字段形同虚设。如果未来支持 PDF/Image，需要在 `write_file` 做 MIME 检测。

### 10. `CollectionTable` 把所有数据类型映射为 `Utf8`

**文件**: `veda-sql/src/collection_table.rs:26-29`

```rust
"int" | "int64" => DataType::Utf8,
"float" | "float64" => DataType::Utf8,
```

SQL 查询中的数值比较 (`WHERE price > 100`) 会按字符串排序执行，产生错误结果（`"9" > "100"`）。

### 11. Milvus `upsert_chunks` 先删后插不原子

**文件**: `veda-store/src/milvus.rs:365-405`

先 delete 再 insert。如果 delete 成功但 insert 失败（网络抖动、Milvus 限流），向量数据就丢了。Worker 会重试整个任务，但在重试之前该文件的搜索结果为空。

### 12. Embedding 批量调用无大小限制

**文件**: `veda-pipeline/src/embedding.rs:89`

`embed(&self, texts: &[String])` 把所有文本一次性发到 embedding API。一个大文件可能产生几百个 semantic chunk，单次 HTTP 请求的 body 可能超过 API 服务的限制（如 OpenAI 有 batch size 上限）。

### 13. `hybrid_search_remote` 吞掉了所有错误

**文件**: `veda-store/src/milvus.rs:304-310`

```rust
Err(_) => Ok(None),
```

如果 Milvus hybrid_search 端点返回错误（认证失败、网络超时等），代码静默回退到普通 ANN search。应该至少 log warning。

### 14. CLI `--password` 在进程列表中可见

**文件**: `veda-cli/src/main.rs:96`

`--password` 作为命令行参数传入，`ps aux` 可以看到。应使用 `rpassword` 做终端输入或从环境变量读取。

### 15. SQL 端点无查询复杂度限制

**文件**: `veda-server/src/routes/sql.rs`

用户可以提交任意复杂的 SQL（大量 JOIN、嵌套子查询），DataFusion 会在内存中执行，可能导致 OOM。应该做超时或限制。

---

## 三、架构缺陷

### 16. `milvus_name` 计算散落在多处

`CollectionService` 的 `create`、`delete`、`insert_rows`、`search` 以及 `CollectionTable::new` 共 5 处重复 `format!("veda_coll_{}", schema.id.replace('-', "_"))`。如果格式变化，很容易漏改。应抽成 `CollectionSchema` 上的方法或独立函数。

### 17. MySQL 枚举 parse/str 函数与 serde 不同源

`types.rs` 中枚举使用 `#[serde(rename_all = "snake_case")]` 自动映射，`mysql.rs` 中手写了一套 `parse_xxx` / `xxx_str` 函数做同样的事。两套映射极易不同步。

**建议**: 让枚举实现 `Display` + `FromStr`，MySQL 层直接调用 `.to_string()` / `.parse()`。

### 18. `veda-types` 声称零依赖但依赖 `serde_json`

`AGENTS.md` 说 `veda-types` 是零依赖的领域类型，但 `Cargo.toml` 依赖了 `serde`、`serde_json`、`thiserror`、`uuid`、`chrono`。`CollectionSchema.schema_json` 和 `OutboxEvent.payload` 字段是 `serde_json::Value`，强制引入了 `serde_json`。

如果想真正零外部依赖，payload 应该用 `String` 存储，在使用侧再解析。

### 19. `MysqlStore` 承担了过多职责

同一个 struct 实现了 `MetadataStore`、`TaskQueue`、`AuthStore`、`CollectionMetaStore` 四个 trait。所有 trait 共享同一个连接池，没有分离的可能。如果某天 Auth 需要独立数据库或 TaskQueue 需要 Redis，改动成本高。

### 20. 无数据库迁移机制

`MysqlStore::migrate()` 使用 `CREATE TABLE IF NOT EXISTS`。这意味着：
- 无法 ALTER 已有表（加列、改索引、改列类型）
- 无法追踪版本
- 开发环境和生产环境的 schema 可能不一致

应该引入 `sqlx-migrate` 或类似机制。

### 21. `FilesTable::scan` 对每个文件做独立 SQL 查询

**文件**: `veda-sql/src/files_table.rs:93-103`

递归 BFS 每个目录、再逐个 `get_file(fid)` 获取 `FileRecord`。一个有 1000 个文件的 workspace 会产生 ~1000 次 SQL 查询。应该用批量查询（`WHERE id IN (...)` 或 JOIN）。

### 22. `SqlResponse` 定义了但 SQL 路由没有用

**文件**: `veda-types/src/api.rs:124-128`

`SqlResponse` 有 `columns` 和 `rows` 字段，但 `routes/sql.rs` 直接返回 `Vec<serde_json::Value>`。类型定义无用。

---

## 四、小问题 / 代码卫生

| # | 位置 | 问题 |
|---|------|------|
| 23 | `veda-server/config.rs:56` | server 默认端口 3000，CLI 默认 9009，不一致 |
| 24 | `veda-core/src/service/fs.rs:32` | `content.lines().count() as i32`：空字符串 `lines()` 返回 0，所以空文件 line_count = Some(0)，但 `"a"` 返回 1、`"a\n"` 也返回 1。最后一行无 `\n` 时计数不含尾行的换行，这个语义需明确 |
| 25 | `veda-types/src/types.rs:231` | `FieldDefinition.field_type` 用 `#[serde(alias = "type")]` 但 `#[serde(rename = "field_type")]` 未设，所以 JSON 输出是 `"field_type"` 而非 `"type"`。schema_json 里存的和 API 文档可能不一致 |
| 26 | `veda-store/src/milvus.rs:399-404` | `upsert_chunks` 最后的 flush 错误被忽略 (`let _ = ...`)。如果 flush 失败，数据不会被持久化 |
| 27 | `veda-core/src/service/fs.rs:573-604` | `split_into_chunks` 和 `veda-pipeline/src/chunking.rs:126` 的 `storage_chunk` 逻辑几乎完全一样。重复代码，应统一到 pipeline crate |
| 28 | `veda-server/src/worker.rs:91-93` | `CollectionSync` 事件直接 complete，没有实际处理逻辑 |

---

## 五、优先级建议

| 优先级 | Issue # | 摘要 |
|--------|---------|------|
| **P0** | 4 | 唯一索引前缀 255 导致长路径冲突 |
| **P0** | 3 | LIKE 模式未转义导致目录操作错误 |
| **P1** | 2 | MockTx 无事务语义，核心逻辑无可靠测试 |
| **P1** | 5 | mkdir 竞态条件 |
| **P1** | 6 | search path_prefix 结果数不可控 |
| **P1** | 10 | SQL 数值比较错误 |
| **P1** | 17 | 枚举映射两套实现可能不同步 |
| **P2** | 1 | read_file_lines 浪费 I/O |
| **P2** | 7 | login 无限生成 API Key |
| **P2** | 11 | Milvus upsert 非原子 |
| **P2** | 12 | Embedding 批量无限制 |
| **P2** | 16 | milvus_name 重复计算 |
| **P2** | 19 | MysqlStore 职责过多 |
| **P2** | 20 | 无迁移机制 |
| **P2** | 21 | FilesTable N+1 查询 |
| **P3** | 其余 | 代码卫生和小改进 |
