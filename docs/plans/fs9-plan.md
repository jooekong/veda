# Veda fs9-style SQL 文件函数实现方案

> 参考 [db9.ai fs9 extension](https://db9.ai/docs/extensions/fs9/)，在 Veda 的 DataFusion SQL 引擎中实现类似的文件操作 SQL 函数，使用户可以直接从 SQL 读写文件、查询目录、解析结构化文件。

---

## 背景

Veda 现状：
- 已有完整的 FS CRUD（write/read/list/stat/delete/copy/rename/mkdir）
- 已有 DataFusion SQL 引擎，注册了 `files` 表（目录树）和 collection 表
- 已有 `fs_events` 表（变更追踪）
- 存储层：MySQL（元数据 + 文件内容）、Milvus（向量搜索）

fs9 核心能力：
- **Scalar Functions** — `fs9_read/write/append/exists/size/mtime/remove/mkdir` 等
- **Table Function** — `fs9('/path/')` 可读目录、读文件为行、glob 展开
- **格式自动检测** — CSV/TSV/JSONL/Parquet → 自动解析为 SQL 列
- **事件查询** — `fs9_events()` cursor-based polling

**关键差异**：fs9 的存储后端是 TiKV（page-based），Veda 的存储是 MySQL（inline/chunked text）。Veda 不需要照搬 fs9 的所有能力，而是选取 **对用户价值最高且与现有架构自然衔接** 的部分。

---

## 实现范围

### 纳入

| 能力 | 优先级 | 理由 |
|------|--------|------|
| SQL Scalar Functions（read/write/append/exists/size/mtime/remove/mkdir） | P0 | SQL 层直接操作文件，核心能力 |
| `veda_fs()` Table Function（目录列举 + 文件读取 + glob） | P0 | 杀手功能，SQL 查询文件内容为行 |
| 格式自动检测（CSV/TSV/JSONL/plain text） | P1 | `veda_fs('/data.csv')` 自动解析列 |
| `veda_fs_events()` Table Function | P1 | 已有 fs_events 表，暴露为 SQL 函数 |
| `veda_storage_stats()` | P2 | 存储统计，实现简单 |
| `fs_append` HTTP API | P2 | REST 层补齐 append 能力 |

### 排除

| 能力 | 理由 |
|------|------|
| `read_at` / `write_at`（字节级随机读写） | Veda 存的是 UTF-8 文本，不是二进制；已有 `?lines=N:M` 按行读取 |
| `truncate` | 文本文件场景用处不大，且与 COW/dedup 机制冲突 |
| Parquet 格式支持 | 需引入 `parquet` crate，增量价值低，后续按需加 |
| Symlink | 超出 Veda 当前文件模型 |
| WebSocket streaming file I/O | 单独的 WebSocket 事件推送已在规划中，文件 I/O 走 REST 即可 |

---

## 技术方案

### Phase A：SQL Scalar Functions（P0）

**目标**：在 `veda-sql` crate 中注册 DataFusion UDF/UDAF，使 SQL 可直接操作文件。

#### A.1 函数清单

| SQL 函数 | 签名 | 语义 | 对应 FsService 方法 |
|----------|------|------|---------------------|
| `veda_read(path)` | `(Utf8) → Utf8` | 读整个文件内容 | `read_file` |
| `veda_write(path, content)` | `(Utf8, Utf8) → Int64` | 写文件，返回字节数 | `write_file` |
| `veda_append(path, content)` | `(Utf8, Utf8) → Int64` | 追加内容，返回字节数 | 新增 `append_file` |
| `veda_exists(path)` | `(Utf8) → Boolean` | 文件/目录是否存在 | `stat`（判断 Ok/Err） |
| `veda_size(path)` | `(Utf8) → Int64` | 文件字节数 | `stat → size_bytes` |
| `veda_mtime(path)` | `(Utf8) → Utf8` | 最后修改时间（RFC 3339） | `stat → updated_at` |
| `veda_remove(path)` | `(Utf8) → Int64` | 删除文件/目录，返回删除数 | `delete_file` / `delete_dir` |
| `veda_mkdir(path)` | `(Utf8) → Boolean` | 创建目录 | `mkdir` |

#### A.2 架构设计

```
SQL: SELECT veda_read('/config/app.toml')
        │
        ▼
    DataFusion UDF
        │
        ▼
    FsService.read_file(workspace_id, "/config/app.toml")
        │
        ▼
    MySQL (dentries + file_contents/file_chunks)
```

**核心问题**：DataFusion UDF 是纯函数（`ScalarUDF`），但 FS 操作需要 `FsService` 实例和 `workspace_id` 上下文。

**解决方案**：利用 DataFusion 的 `SessionContext` 运行时配置注入上下文。

```rust
// veda-sql/src/udf/mod.rs

pub struct FsUdfContext {
    pub workspace_id: String,
    pub fs_service: Arc<FsService<...>>,
}

// 注册 UDF 时，将 FsUdfContext 通过 Arc 注入闭包
fn register_fs_udfs(ctx: &SessionContext, fs_ctx: Arc<FsUdfContext>) {
    ctx.register_udf(make_veda_read(fs_ctx.clone()));
    ctx.register_udf(make_veda_write(fs_ctx.clone()));
    ctx.register_udf(make_veda_exists(fs_ctx.clone()));
    // ...
}
```

**UDF 实现模式**（以 `veda_read` 为例）：

```rust
fn make_veda_read(fs_ctx: Arc<FsUdfContext>) -> ScalarUDF {
    let udf = ScalarUDF::from(VedaReadUdf { fs_ctx });
    udf
}

#[derive(Debug)]
struct VedaReadUdf {
    fs_ctx: Arc<FsUdfContext>,
}

impl ScalarUDFImpl for VedaReadUdf {
    fn name(&self) -> &str { "veda_read" }

    fn return_type(&self, _args: &[DataType]) -> Result<DataType> {
        Ok(DataType::Utf8)
    }

    fn invoke_batch(&self, args: &[ColumnarValue], num_rows: usize) -> Result<ColumnarValue> {
        // 1. 从 args[0] 提取 path
        // 2. 用 tokio::runtime::Handle::current().block_on() 调用 async FsService
        //    （DataFusion UDF 是同步的，但运行在 tokio 上下文中）
        // 3. 返回 StringArray
    }
}
```

**注意**：DataFusion 的 `invoke_batch` 是同步的。由于 Veda 的 SQL 执行已经在 tokio 运行时中（从 Axum handler 调用），可以用 `tokio::task::block_in_place` + `Handle::current().block_on()` 桥接 async 调用。这在 multi-thread runtime 下是安全的。

#### A.3 写操作的 Side Effects

`veda_write` 和 `veda_append` 是有副作用的 SQL 函数。需要注意：

- **outbox 触发**：写入文件后会触发 `ChunkSync` outbox，这是期望的行为（保持与 REST API 一致）
- **事务边界**：每次 UDF 调用是独立事务，不支持跨多个 UDF 调用的原子操作
- **幂等性**：`veda_write` 有 SHA256 去重，重复写入不会产生副作用

#### A.4 `append_file` 新增

`FsService` 目前没有 append 操作，需要新增：

```rust
// veda-core/src/service/fs.rs

impl FsService {
    pub async fn append_file(
        &self,
        workspace_id: &str,
        path: &str,
        content: &str,
    ) -> Result<WriteFileResponse> {
        // 1. 读取现有文件内容（若存在）
        // 2. 拼接新内容
        // 3. 调用 write_file（复用去重、分层存储、outbox 逻辑）
        //
        // 注意：这是 read-modify-write，不是真正的 atomic append。
        // 对于 Veda 的 text 文件场景（日志追加、JSONL 写入）足够用。
        // 如果文件不存在，等同于 write_file。
    }
}
```

---

### Phase B：`veda_fs()` Table Function（P0）

**目标**：注册 DataFusion Table Function，支持三种模式。

#### B.1 三种模式

| 模式 | 触发条件 | 返回 schema | 示例 |
|------|----------|-------------|------|
| **目录列举** | path 以 `/` 结尾 | `path, type, size, mtime` | `SELECT * FROM veda_fs('/logs/')` |
| **文件读取** | path 指向文件 | 根据格式自动检测 | `SELECT * FROM veda_fs('/data/users.csv')` |
| **glob 匹配** | path 含 `*`, `?`, `[` | 同文件读取（多文件联合） | `SELECT * FROM veda_fs('/logs/*.jsonl')` |

#### B.2 实现为 DataFusion TableProvider

```rust
// veda-sql/src/fs_table.rs

pub struct VedaFsTableProvider {
    workspace_id: String,
    fs_service: Arc<FsService<...>>,
    path: String,
    options: FsTableOptions,
}

struct FsTableOptions {
    recursive: bool,
    format: Option<String>,     // "csv", "tsv", "jsonl", "text"
    delimiter: Option<char>,
    header: Option<bool>,
    exclude: Option<String>,
}
```

注册为 DataFusion 的 table-valued function：

```rust
// engine.rs — 注册 veda_fs 函数

ctx.register_udtf("veda_fs", Arc::new(VedaFsTableFactory {
    workspace_id: workspace_id.clone(),
    fs_service: fs_service.clone(),
}));
```

#### B.3 格式自动检测

```rust
fn detect_format(path: &str) -> FileFormat {
    match path.rsplit('.').next() {
        Some("csv") => FileFormat::Csv { delimiter: b',', header: true },
        Some("tsv") => FileFormat::Tsv,
        Some("jsonl") | Some("ndjson") => FileFormat::JsonLines,
        _ => FileFormat::PlainText,
    }
}
```

各格式返回的 schema：

| 格式 | 列 |
|------|-----|
| CSV/TSV | `_line_number: Int64`, 从 header 推断的列..., `_path: Utf8` |
| JSONL | `_line_number: Int64`, `line: Utf8` (JSON string), `_path: Utf8` |
| Plain Text | `_line_number: Int64`, `line: Utf8`, `_path: Utf8` |

**CSV 列推断**：读取第一行作为 header，所有列类型默认为 `Utf8`。后续可加类型推断（扫描前 N 行）。

#### B.4 Glob 实现

```rust
// veda-core/src/path.rs (或新模块)

pub fn glob_match(pattern: &str, path: &str) -> bool {
    // 简单 glob：* 匹配非 / 字符，** 匹配任意路径，? 匹配单字符
    // 可用 `glob-match` crate 或手写
}

// FsService 新增
pub async fn glob_files(
    &self,
    workspace_id: &str,
    pattern: &str,
) -> Result<Vec<DirEntry>> {
    // 1. 提取 pattern 的固定前缀（如 /logs/ 从 /logs/*.jsonl）
    // 2. list_dir(recursive=true) 从前缀开始
    // 3. 用 glob_match 过滤
}
```

#### B.5 查询示例

```sql
-- 列目录
SELECT path, type, size, mtime FROM veda_fs('/docs/') ORDER BY path;

-- 读 CSV 文件
SELECT name, age FROM veda_fs('/data/users.csv') WHERE age > 18;

-- 读 JSONL 日志
SELECT line->>'level' AS level, count(*)
FROM veda_fs('/logs/app.jsonl')
GROUP BY 1;

-- glob 查询所有 CSV
SELECT _path, count(*) AS rows
FROM veda_fs('/data/*.csv')
GROUP BY _path;

-- 结合 embedding search
SELECT f.path, f.line
FROM veda_fs('/logs/*.jsonl') f
WHERE f.line LIKE '%error%';
```

---

### Phase C：格式解析器（P1）

**目标**：实现 CSV/TSV/JSONL/plain text 四种格式的文件到 Arrow RecordBatch 转换。

#### C.1 模块结构

```
veda-sql/src/
├── format/
│   ├── mod.rs          // FileFormat enum + detect_format()
│   ├── csv.rs          // CSV/TSV → RecordBatch
│   ├── jsonl.rs        // JSON Lines → RecordBatch
│   └── text.rs         // Plain text → RecordBatch
```

#### C.2 CSV 解析

```rust
// 使用 arrow-csv crate（已在 DataFusion 依赖树中）
use arrow_csv::ReaderBuilder;

pub fn parse_csv(
    content: &str,
    delimiter: u8,
    has_header: bool,
    path: &str,
) -> Result<RecordBatch> {
    let cursor = std::io::Cursor::new(content.as_bytes());
    let reader = ReaderBuilder::new(SchemaRef::from(inferred_schema))
        .with_delimiter(delimiter)
        .with_header(has_header)
        .build(cursor)?;
    // 读取所有行，附加 _line_number 和 _path 列
}
```

#### C.3 JSONL 解析

```rust
pub fn parse_jsonl(content: &str, path: &str) -> Result<RecordBatch> {
    let mut line_numbers = Vec::new();
    let mut json_strings = Vec::new();

    for (i, line) in content.lines().enumerate() {
        if line.trim().is_empty() { continue; }
        // 验证是合法 JSON，跳过非法行
        if serde_json::from_str::<serde_json::Value>(line).is_ok() {
            line_numbers.push((i + 1) as i64);
            json_strings.push(line.to_string());
        }
    }
    // 构建 RecordBatch: _line_number, line, _path
}
```

#### C.4 内存保护

参考 fs9 的 100MB glob read budget：

```rust
const MAX_SINGLE_FILE_READ: usize = 50 * 1024 * 1024;  // 50MB
const MAX_GLOB_TOTAL_READ: usize = 100 * 1024 * 1024;   // 100MB
const MAX_GLOB_FILE_COUNT: usize = 10_000;
```

超出限制返回 `VedaError::QuotaExceeded`。

---

### Phase D：`veda_fs_events()` Table Function（P1）

**目标**：将已有的 `fs_events` MySQL 表暴露为可从 SQL 查询的 table function。

#### D.1 函数签名

```sql
-- 所有事件
SELECT * FROM veda_fs_events();

-- 指定游标之后的事件
SELECT * FROM veda_fs_events(since_id => 1000);

-- 指定路径前缀
SELECT * FROM veda_fs_events(since_id => 0, path_prefix => '/logs/');

-- 指定 limit
SELECT * FROM veda_fs_events(since_id => 0, path_prefix => '/logs/', limit => 100);
```

#### D.2 返回 Schema

| 列 | 类型 | 说明 |
|----|------|------|
| `id` | Int64 | 事件自增 ID |
| `event_type` | Utf8 | `create`, `update`, `delete`, `move` |
| `path` | Utf8 | 受影响路径 |
| `file_id` | Utf8 | 文件 ID（删除时可能为空） |
| `created_at` | Utf8 | 事件时间（RFC 3339） |

#### D.3 实现

直接查询 MySQL `fs_events` 表，转换为 Arrow RecordBatch：

```rust
pub struct VedaFsEventsTableProvider {
    workspace_id: String,
    metadata_store: Arc<dyn MetadataStore>,
    since_id: i64,
    path_prefix: Option<String>,
    limit: usize,
}
```

需要在 `MetadataStore` trait 新增：

```rust
async fn query_fs_events(
    &self,
    workspace_id: &str,
    since_id: i64,
    path_prefix: Option<&str>,
    limit: usize,
) -> Result<Vec<FsEvent>>;
```

---

### Phase E：`veda_storage_stats()` + `append` API（P2）

#### E.1 `veda_storage_stats()`

```sql
SELECT total_files, total_directories, total_bytes FROM veda_storage_stats();
```

实现：在 `MetadataStore` 新增聚合查询：

```sql
SELECT
    COUNT(CASE WHEN is_dir = false THEN 1 END) AS total_files,
    COUNT(CASE WHEN is_dir = true THEN 1 END) AS total_directories,
    COALESCE(SUM(f.size_bytes), 0) AS total_bytes
FROM dentries d
LEFT JOIN files f ON d.file_id = f.id
WHERE d.workspace_id = ?
```

#### E.2 `fs_append` HTTP API

```
POST /v1/fs/{path}?append
Content-Type: text/plain

{"event":"click","ts":"2026-04-16T10:00:00Z"}
```

在 `crates/veda-server/src/routes/fs.rs` 中新增路由逻辑，检测 `?append` 查询参数，调用 `FsService::append_file`。

---

## 实施计划

### Sprint 1：SQL Scalar Functions（约 3-4 天）

| # | 任务 | 涉及 crate |
|---|------|-----------|
| 1 | 新增 `FsService::append_file` 方法 | `veda-core` |
| 2 | 新增 `MetadataStore::query_fs_events` + `storage_stats` | `veda-core`, `veda-store` |
| 3 | 实现 `FsUdfContext` + 注入机制 | `veda-sql` |
| 4 | 实现 8 个 scalar UDF（read/write/append/exists/size/mtime/remove/mkdir） | `veda-sql` |
| 5 | 修改 `VedaSqlEngine::execute` 传入 `FsService` | `veda-sql`, `veda-server` |
| 6 | 单元测试（mock FsService） | `veda-sql` |

### Sprint 2：Table Function + 格式解析（约 4-5 天）

| # | 任务 | 涉及 crate |
|---|------|-----------|
| 7 | 实现 `detect_format` + plain text parser | `veda-sql` |
| 8 | 实现 CSV/TSV parser（arrow-csv） | `veda-sql` |
| 9 | 实现 JSONL parser | `veda-sql` |
| 10 | 实现 `VedaFsTableProvider`（目录列举模式） | `veda-sql` |
| 11 | 实现 `VedaFsTableProvider`（文件读取模式 + 格式检测） | `veda-sql` |
| 12 | 实现 glob 匹配 + `FsService::glob_files` | `veda-core`, `veda-sql` |
| 13 | 注册 `veda_fs` UDTF 到 engine | `veda-sql` |
| 14 | 内存保护（quota limit） | `veda-sql` |
| 15 | 单元测试 + 集成测试 | `veda-sql` |

### Sprint 3：Events + Stats + Append API（约 2 天）

| # | 任务 | 涉及 crate |
|---|------|-----------|
| 16 | 实现 `VedaFsEventsTableProvider` | `veda-sql` |
| 17 | 实现 `VedaStorageStatsTableProvider` | `veda-sql` |
| 18 | 新增 `POST /v1/fs/{path}?append` 路由 | `veda-server` |
| 19 | CLI 新增 `veda append` 命令 | `veda-cli` |
| 20 | 端到端测试 | 全部 |

### Sprint 4：稳定化（约 1-2 天）

| # | 任务 |
|---|------|
| 21 | 错误处理完善（UDF 中的错误 → 友好消息） |
| 22 | 大文件保护测试 |
| 23 | 更新 ARCHITECTURE.md 和 PLANS.md |
| 24 | 更新 CLI help 文档 |

**总计：约 10-13 天**

---

## 风险与对策

| 风险 | 影响 | 对策 |
|------|------|------|
| DataFusion UDF 是同步的，但 FS 操作是 async | 可能阻塞 tokio 线程 | 用 `block_in_place` + `block_on`；写操作串行执行，读操作并发安全 |
| `veda_write` 在 SELECT 中有副作用 | 违反 SQL 纯函数语义 | 文档明确说明；DataFusion 不会自动缓存/优化掉 UDF 调用 |
| CSV header 推断可能误判 | 列名/类型不准 | 支持 `format`, `delimiter`, `header` 参数覆盖自动检测 |
| glob 匹配大目录性能差 | 目录树全扫描 | 利用固定前缀剪枝；加 quota limit |
| `append_file` 是 read-modify-write | 并发 append 可能丢失数据 | MySQL 事务保证单个 append 原子；文档说明不适合高并发 append |

---

## 与 fs9 的差异对照

| fs9 能力 | Veda 实现 | 差异说明 |
|----------|-----------|---------|
| `fs9_read` | `veda_read` | 相同语义 |
| `fs9_write` | `veda_write` | 相同语义，额外有 SHA256 去重 |
| `fs9_append` | `veda_append` | read-modify-write 而非 page-level append |
| `fs9_read_at` / `fs9_write_at` | 不实现 | 用 `?lines=N:M` 替代；text 场景不需要字节级随机访问 |
| `fs9_truncate` | 不实现 | 用 `veda_write(path, '')` 替代 |
| `fs9_exists/size/mtime` | `veda_exists/size/mtime` | 相同语义 |
| `fs9_remove` | `veda_remove` | 支持文件/目录，暂不支持 glob 删除 |
| `fs9_mkdir` | `veda_mkdir` | 相同语义 |
| `fs9()` table function | `veda_fs()` | 三种模式（dir/file/glob）相同 |
| 格式检测 | CSV/TSV/JSONL/text | 不含 Parquet |
| `fs9_events()` | `veda_fs_events()` | cursor-based polling，语义相同 |
| `fs9_storage_stats()` | `veda_storage_stats()` | 相同 |
| FUSE mount | `veda-fuse`（已规划） | 独立 crate，不在本方案范围 |
| WebSocket API | 已规划，不在本方案 | 本方案聚焦 SQL 层 |
| Symlink | 不实现 | 超出 Veda 文件模型 |

---

## 验收标准

| 场景 | SQL 示例 | 期望结果 |
|------|----------|---------|
| 读文件 | `SELECT veda_read('/app/config.toml')` | 返回文件完整内容 |
| 写文件 | `SELECT veda_write('/tmp/out.txt', 'hello')` | 返回 5，文件已创建 |
| 追加内容 | `SELECT veda_append('/logs/app.log', 'new line\n')` | 返回追加字节数 |
| 检查存在 | `SELECT veda_exists('/app/config.toml')` | 返回 true |
| 目录列举 | `SELECT * FROM veda_fs('/docs/')` | 返回 path, type, size, mtime 列 |
| CSV 查询 | `SELECT name FROM veda_fs('/data/users.csv') WHERE age > 18` | 正确解析 CSV 列并过滤 |
| JSONL 查询 | `SELECT line FROM veda_fs('/logs/app.jsonl') LIMIT 10` | 每行是 JSON 字符串 |
| glob 查询 | `SELECT count(*) FROM veda_fs('/logs/*.jsonl')` | 统计所有 JSONL 文件行数 |
| 事件查询 | `SELECT * FROM veda_fs_events(since_id => 100)` | 返回 ID > 100 的事件 |
| 存储统计 | `SELECT * FROM veda_storage_stats()` | 返回文件数、目录数、总字节数 |
