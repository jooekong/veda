# Veda 设计文档

> 基于 vecfs 重做，沿用 MySQL + Milvus 技术栈，解决双写一致性、模块耦合、多租户等问题。

---

## 设计约束


| 维度     | 决策                                       |
| ------ | ---------------------------------------- |
| 规模     | 中型团队（5-50人），10万-100万文件，需要高可用             |
| 部署     | Kubernetes                               |
| 文件类型   | 文本 + PDF/图片（提取文本，**不存原始二进制**）            |
| 一致性    | 最终一致 + 异常 Outbox 自愈，不做分布式强一致             |
| 多租户    | 共享数据库，行级隔离（`workspace_id`）               |
| Worker | 内嵌 tokio task（可配置禁用，独立部署）                |
| API    | REST + WebSocket（文件系统事件）                 |
| SQL 引擎 | DataFusion                               |
| 存储     | 不引入 S3，文本存 MySQL，语义 chunk 存 Milvus       |
| 测试     | trait 抽象 + mock 单测 + testcontainers 集成测试 |


---

## 1. 模块架构

### 1.1 Crate 结构（8 crates）

```
veda/
├── crates/
│   ├── veda-types/      零依赖：领域类型、错误定义、API DTO
│   ├── veda-core/       trait 定义 + 业务逻辑（不依赖具体存储实现）
│   ├── veda-store/      MySQL + Milvus 的 trait 实现
│   ├── veda-pipeline/   embedding、chunking、PDF/OCR 提取
│   ├── veda-sql/        DataFusion SQL 引擎（隔离 Arrow 重编译）
│   ├── veda-server/     Axum HTTP 层（薄壳：路由 + 中间件 + 启动）
│   ├── veda-cli/        CLI 客户端（纯 HTTP，不直接连数据库）
│   └── veda-fuse/       FUSE 挂载（可选，不在默认 workspace 中）
```

### 1.2 依赖关系

```
veda-types          (零外部依赖，只有 serde/thiserror/uuid/chrono)
    ↑
veda-core           (依赖 veda-types + async-trait + sha2)
    ↑
├── veda-store      (依赖 veda-core + sqlx + reqwest)
├── veda-pipeline   (依赖 veda-core + reqwest + pdf-extract)
└── veda-sql        (依赖 veda-core + datafusion + arrow)
        ↑
    veda-server     (依赖 store + pipeline + sql + axum)
```

### 1.3 拆分理由


| Crate           | 拆分收益                                   |
| --------------- | -------------------------------------- |
| `veda-types`    | 零编译依赖，CLI/FUSE 只依赖它，不拉存储层              |
| `veda-core`     | 业务逻辑可独立单测（mock trait），不需要 MySQL/Milvus |
| `veda-store`    | 隔离 `sqlx` + Milvus HTTP 客户端，可独立集成测试    |
| `veda-pipeline` | 隔离 `pdf-extract`/OCR 重依赖，改路由不触发重编译     |
| `veda-sql`      | 隔离 DataFusion + Arrow（占编译时间 60%+）      |
| `veda-server`   | 薄壳，只做 Axum 路由/中间件/启动                   |


---

## 2. 认证体系

参考 db9.ai 的 Customer → Database 两级模型，映射为 Account → Workspace。

### 2.1 概念映射


| db9           | Veda                  | 说明                       |
| ------------- | --------------------- | ------------------------ |
| Customer      | Account               | 顶层用户/组织，可自助注册            |
| Database      | Workspace             | 独立的文件树 + 向量空间            |
| API Token     | API Key               | 账户级管理凭证（创建/删除 workspace） |
| Connect Token | Workspace Token (JWT) | 数据面凭证（文件 CRUD、搜索）        |
| Connect Key   | Workspace Key         | 长期数据面凭证（CI/agent 用）      |


### 2.2 凭证体系

```
Account (joe@example.com)
├── API Key: veda_ak_xxxx...  (长期，管控面)
│   可以: 创建/删除 workspace, 管理 key, 查看 usage
│
├── Workspace: my-project
│   ├── Workspace Key: veda_wk_xxxx...  (长期，数据面，用于 CI/agent)
│   └── Workspace Token: JWT            (短期 24h，数据面，用于交互)
│       可以: 文件 CRUD, 搜索, collection 操作
│
└── Workspace: notes
    ├── Workspace Key: veda_wk_yyyy...
    └── Workspace Token: JWT
```

### 2.3 认证流程

**管控面（Account Management）：**

```
POST /v1/accounts             → 注册，返回 account_id + API Key
POST /v1/accounts/login       → 登录，返回 API Key
POST /v1/workspaces           → 用 API Key 创建 workspace
DELETE /v1/workspaces/{id}    → 用 API Key 删除 workspace
```

**数据面（Workspace Operations）：**

```
POST /v1/workspaces/{id}/token   → 用 API Key 换取短期 JWT
PUT /v1/fs/{path}                → 用 JWT 或 Workspace Key 写文件
POST /v1/search                  → 用 JWT 或 Workspace Key 搜索
```

**Key 存储方式：**

- API Key / Workspace Key 都是 `SHA256(random_bytes)` 生成
- DB 中存 `SHA256(key)` hash，不存明文
- 验证时 `SHA256(request_key) == stored_hash`

---

## 3. MySQL Schema

### 3.1 Control Plane

```sql
CREATE TABLE accounts (
    id              VARCHAR(36) PRIMARY KEY,
    name            VARCHAR(128) NOT NULL,
    email           VARCHAR(256) UNIQUE,
    password_hash   VARCHAR(128),           -- argon2, NULL = anonymous
    status          ENUM('active','suspended') DEFAULT 'active',
    created_at      TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    updated_at      TIMESTAMP DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP
);

CREATE TABLE api_keys (
    id              VARCHAR(36) PRIMARY KEY,
    account_id      VARCHAR(36) NOT NULL REFERENCES accounts(id),
    name            VARCHAR(128) NOT NULL,
    key_hash        VARCHAR(64) NOT NULL,   -- SHA256(明文 key)
    status          ENUM('active','revoked') DEFAULT 'active',
    created_at      TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    UNIQUE INDEX idx_key_hash (key_hash)
);

CREATE TABLE workspaces (
    id              VARCHAR(36) PRIMARY KEY,
    account_id      VARCHAR(36) NOT NULL REFERENCES accounts(id),
    name            VARCHAR(128) NOT NULL,
    status          ENUM('active','archived') DEFAULT 'active',
    created_at      TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    updated_at      TIMESTAMP DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
    UNIQUE INDEX idx_account_name (account_id, name)
);

CREATE TABLE workspace_keys (
    id              VARCHAR(36) PRIMARY KEY,
    workspace_id    VARCHAR(36) NOT NULL REFERENCES workspaces(id),
    name            VARCHAR(128) NOT NULL,
    key_hash        VARCHAR(64) NOT NULL,   -- SHA256(明文 key)
    permission      ENUM('read','readwrite') DEFAULT 'readwrite',
    status          ENUM('active','revoked') DEFAULT 'active',
    created_at      TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    UNIQUE INDEX idx_key_hash (key_hash)
);
```

### 3.2 File System（Data Plane）

```sql
CREATE TABLE dentries (
    id              VARCHAR(36) PRIMARY KEY,
    workspace_id    VARCHAR(36) NOT NULL,
    parent_path     VARCHAR(4096) NOT NULL,
    name            VARCHAR(255) NOT NULL,
    path            VARCHAR(4096) NOT NULL,
    path_hash       VARCHAR(64) AS (SHA2(CONCAT(workspace_id, ':', path), 256)) STORED,
    file_id         VARCHAR(36),            -- NULL = directory
    is_dir          BOOLEAN NOT NULL DEFAULT FALSE,
    created_at      TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    updated_at      TIMESTAMP DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
    UNIQUE INDEX idx_path_hash (path_hash),
    INDEX idx_parent (workspace_id, parent_path)
);

CREATE TABLE files (
    id              VARCHAR(36) PRIMARY KEY,
    workspace_id    VARCHAR(36) NOT NULL,
    size_bytes      BIGINT NOT NULL DEFAULT 0,
    mime_type       VARCHAR(128) DEFAULT 'text/plain',
    storage_type    ENUM('inline','chunked') NOT NULL DEFAULT 'inline',
    source_type     ENUM('text','pdf','image') NOT NULL DEFAULT 'text',
    line_count      INT,                    -- 总行数（text 文件）
    checksum_sha256 VARCHAR(64) NOT NULL,   -- 内容指纹，用于去重
    revision        INT NOT NULL DEFAULT 1,
    ref_count       INT NOT NULL DEFAULT 1, -- COW 引用计数
    created_at      TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    updated_at      TIMESTAMP DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
    INDEX idx_workspace (workspace_id),
    INDEX idx_checksum (workspace_id, checksum_sha256)
);

-- 小文件（≤256KB）内容，inline 单行存储
CREATE TABLE file_contents (
    file_id         VARCHAR(36) PRIMARY KEY REFERENCES files(id),
    content         LONGTEXT NOT NULL
);

-- 大文件（>256KB）分块存储
CREATE TABLE file_chunks (
    file_id         VARCHAR(36) NOT NULL,
    chunk_index     INT NOT NULL,
    start_line      INT NOT NULL,           -- 本块首行行号 (1-based)
    content         MEDIUMTEXT NOT NULL,
    PRIMARY KEY (file_id, chunk_index),
    INDEX idx_line_lookup (file_id, start_line)
);
```

### 3.3 Outbox（替代 vecfs 的 `semantic_tasks`）

```sql
CREATE TABLE outbox (
    id              BIGINT AUTO_INCREMENT PRIMARY KEY,
    workspace_id    VARCHAR(36) NOT NULL,
    event_type      ENUM('chunk_sync','chunk_delete','collection_sync') NOT NULL,
    payload         JSON NOT NULL,
    status          ENUM('pending','processing','completed','failed','dead') DEFAULT 'pending',
    retry_count     INT NOT NULL DEFAULT 0,
    max_retries     INT NOT NULL DEFAULT 5,
    available_at    TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    lease_until     TIMESTAMP NULL,
    created_at      TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    INDEX idx_claim (status, available_at)
);
```

### 3.4 Collection Schema（元数据）

```sql
CREATE TABLE collection_schemas (
    id              VARCHAR(36) PRIMARY KEY,
    workspace_id    VARCHAR(36) NOT NULL,
    name            VARCHAR(128) NOT NULL,
    collection_type ENUM('structured','raw') NOT NULL DEFAULT 'structured',
    schema_json     JSON NOT NULL,          -- 字段定义: name, type, index, embed
    embedding_source VARCHAR(128),          -- 哪个字段做 embedding
    embedding_dim   INT,
    status          ENUM('active','deleting') DEFAULT 'active',
    created_at      TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    updated_at      TIMESTAMP DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
    UNIQUE INDEX idx_ws_name (workspace_id, name)
);
```

### 3.5 WebSocket 事件

```sql
CREATE TABLE fs_events (
    id              BIGINT AUTO_INCREMENT PRIMARY KEY,
    workspace_id    VARCHAR(36) NOT NULL,
    event_type      ENUM('create','update','delete','move') NOT NULL,
    path            VARCHAR(4096) NOT NULL,
    file_id         VARCHAR(36),
    created_at      TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    INDEX idx_ws_poll (workspace_id, id)
);
```

---

## 4. Milvus Collections

### 4.1 文件语义 Chunks

Collection: `veda_chunks`


| 字段              | 类型                  | 说明                 |
| --------------- | ------------------- | ------------------ |
| `id`            | VARCHAR(36) PK      | chunk ID           |
| `workspace_id`  | VARCHAR(36)         | partition key      |
| `file_id`       | VARCHAR(36)         | 所属文件               |
| `chunk_index`   | INT                 | 语义块序号              |
| `content`       | VARCHAR(65535)      | 文本内容               |
| `vector`        | FLOAT_VECTOR(dim)   | dense embedding    |
| `sparse_vector` | SPARSE_FLOAT_VECTOR | BM25 sparse vector |


索引：HNSW (dense) + SPARSE_INVERTED_INDEX (sparse) + BM25 function

### 4.2 Structured Collection

Collection: `veda_coll_{collection_id}`

动态 schema，由用户定义。示例：

```
collection create articles --field "title:string:index" --field "content:string:embed" --field "category:string:index"
```

生成的 Milvus collection:


| 字段              | 类型                  | 说明               |
| --------------- | ------------------- | ---------------- |
| `id`            | VARCHAR(36) PK      | row ID           |
| `workspace_id`  | VARCHAR(36)         | partition key    |
| `title`         | VARCHAR(65535)      | 用户字段，带 index     |
| `content`       | VARCHAR(65535)      | 用户字段，标记为 embed 源 |
| `category`      | VARCHAR(65535)      | 用户字段，带 index     |
| `vector`        | FLOAT_VECTOR(dim)   | 从 content 字段同步生成 |
| `sparse_vector` | SPARSE_FLOAT_VECTOR | BM25             |


**关键：Structured Collection 数据只存 Milvus，MySQL 只存 `collection_schemas` 元数据。**

embedding 是**同步**的（INSERT 时立即调用 embedding API），不走 outbox，因此没有双写一致性问题。

### 4.3 Raw Vector Collection

Collection: `veda_vectors_{dim}`


| 字段              | 类型                | 说明            |
| --------------- | ----------------- | ------------- |
| `id`            | VARCHAR(36) PK    | vector ID     |
| `workspace_id`  | VARCHAR(36)       | partition key |
| `collection_id` | VARCHAR(36)       | 逻辑分组          |
| `vector`        | FLOAT_VECTOR(dim) | 用户提供的向量       |
| `payload`       | JSON              | 用户附加数据        |


---

## 5. 文件存储设计

### 5.1 分层存储

```
写入文件
    │
    ├── size ≤ 256KB?
    │   YES → storage_type = 'inline'
    │         写 file_contents 表（单行 LONGTEXT）
    │
    └── size > 256KB?
        YES → storage_type = 'chunked'
              按 ~256KB 边界切块（对齐换行符）
              写 file_chunks 表（每块一行 MEDIUMTEXT）
              每块记录 start_line
```

### 5.2 Content-Addressed Dedup

```
PUT /v1/fs/main.rs (content = "fn main() {}")
    │
    ├── 计算 checksum = SHA256(content)
    ├── 查已有文件的 checksum_sha256
    │
    ├── checksum 相同？
    │   YES → 返回当前 revision，什么都不做
    │         HTTP 200，响应头 X-Content-Unchanged: true
    │         不写 DB，不触发 outbox，不发 fs_event
    │
    └── checksum 不同？
        └── 正常写入流程
```

节省的资源：


| 跳过的操作                       | 说明                              |
| --------------------------- | ------------------------------- |
| UPDATE files                | 不更新 revision、content、updated_at |
| file_contents / file_chunks | 不写内容                            |
| INSERT outbox               | 不触发 re-embedding                |
| INSERT fs_events            | 不发 WebSocket 事件                 |
| Milvus upsert               | 不触发向量重建                         |


### 5.3 按行号读取

大文件（chunked）按行号读取流程：

```
GET /v1/fs/main.rs?lines=6500-6510

1. SELECT chunk_index, start_line, content
   FROM file_chunks
   WHERE file_id = ? AND start_line <= 6510
   ORDER BY chunk_index

2. 利用 start_line 索引，跳过不需要的块

3. 从定位到的块中提取目标行范围

4. 如果行范围跨块，拼接相邻块内容
```

`files.line_count` 用于快速验证行号范围是否合法。

---

## 6. 一致性设计

### 6.1 正常路径：Outbox 最终一致

文件写入流程：

```
BEGIN TX
    INSERT/UPDATE files
    INSERT file_contents / file_chunks
    INSERT outbox (event_type = 'chunk_sync', status = 'pending')
COMMIT

Worker 轮询:
    SELECT FROM outbox WHERE status = 'pending' AND available_at <= NOW()
    FOR UPDATE SKIP LOCKED

    → 调用 embedding API
    → upsert Milvus veda_chunks
    → UPDATE outbox SET status = 'completed'
```

文件删除流程：

```
BEGIN TX
    DELETE dentries
    UPDATE files SET ref_count = ref_count - 1
    IF ref_count = 0:
        INSERT outbox (event_type = 'chunk_delete')
        DELETE file_contents / file_chunks
        DELETE files
COMMIT

Worker:
    → DELETE FROM Milvus WHERE file_id = ?
    → UPDATE outbox SET status = 'completed'
```

### 6.2 异常路径：自愈 Reconciler

```
每 5 分钟执行一次:

1. 扫描 outbox 中 status = 'processing' 且 lease_until < NOW() 的任务
   → 重置为 pending（防止 worker crash 导致的卡死）

2. 扫描 retry_count >= max_retries 的任务
   → 标记为 'dead'，记录告警

3. 对比 MySQL files 与 Milvus veda_chunks
   → MySQL 有但 Milvus 没有：补发 chunk_sync outbox
   → Milvus 有但 MySQL 没有：补发 chunk_delete outbox
```

### 6.3 Structured Collection：同步一致

Structured Collection 的数据只存 Milvus，INSERT 时同步调用 embedding API：

```
POST /v1/collections/{name}/rows
    │
    ├── 调用 embedding API（同步）
    ├── Milvus upsert（同步）
    └── 返回 200
```

没有双写，没有 outbox，没有最终一致性问题。
代价是写入延迟更高（等 embedding API），但对结构化数据来说可以接受。

---

## 7. Core Traits

```rust
// veda-core/src/store.rs

#[async_trait]
pub trait MetadataStore: Send + Sync {
    async fn get_dentry(&self, workspace_id: &str, path: &str) -> Result<Option<Dentry>>;
    async fn insert_dentry(&self, dentry: &Dentry) -> Result<()>;
    async fn delete_dentry(&self, workspace_id: &str, path: &str) -> Result<u64>;

    async fn begin_tx(&self) -> Result<Box<dyn MetadataTx>>;
}

#[async_trait]
pub trait MetadataTx: Send {
    async fn get_dentry(&mut self, workspace_id: &str, path: &str) -> Result<Option<Dentry>>;
    async fn insert_file(&mut self, file: &FileRecord) -> Result<()>;
    async fn insert_file_content(&mut self, file_id: &str, content: &str) -> Result<()>;
    async fn insert_file_chunks(&mut self, chunks: Vec<FileChunk>) -> Result<()>;

    async fn commit(self: Box<Self>) -> Result<()>;
    async fn rollback(self: Box<Self>) -> Result<()>;
}

#[async_trait]
pub trait VectorStore: Send + Sync {
    async fn upsert_chunks(&self, chunks: Vec<ChunkWithEmbedding>) -> Result<()>;
    async fn delete_chunks(&self, workspace_id: &str, file_id: &str) -> Result<()>;
    async fn search(&self, req: &SearchRequest) -> Result<Vec<SearchHit>>;
    async fn hybrid_search(&self, req: &HybridSearchRequest) -> Result<Vec<SearchHit>>;
}

#[async_trait]
pub trait TaskQueue: Send + Sync {
    async fn enqueue(&self, task: OutboxEvent) -> Result<()>;
    async fn claim(&self, batch_size: usize) -> Result<Vec<OutboxEvent>>;
    async fn complete(&self, task_id: i64) -> Result<()>;
    async fn fail(&self, task_id: i64, error: &str) -> Result<()>;
}

#[async_trait]
pub trait EmbeddingService: Send + Sync {
    async fn embed(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>>;
    fn dimension(&self) -> usize;
}
```

---

## 8. 错误处理

统一 `VedaError` + `AppError` 模式，替代 vecfs 中散落的 `.map_err`：

```rust
// veda-types/src/errors.rs

#[derive(Debug, thiserror::Error)]
pub enum VedaError {
    #[error("not found: {0}")]
    NotFound(String),

    #[error("already exists: {0}")]
    AlreadyExists(String),

    #[error("permission denied")]
    PermissionDenied,

    #[error("invalid path: {0}")]
    InvalidPath(String),

    #[error("quota exceeded: {0}")]
    QuotaExceeded(String),

    #[error("embedding failed: {0}")]
    EmbeddingFailed(String),

    #[error("storage error: {0}")]
    Storage(#[from] anyhow::Error),
}

// veda-server 中
pub struct AppError(VedaError);

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = match &self.0 {
            VedaError::NotFound(_) => StatusCode::NOT_FOUND,
            VedaError::AlreadyExists(_) => StatusCode::CONFLICT,
            VedaError::PermissionDenied => StatusCode::FORBIDDEN,
            VedaError::InvalidPath(_) => StatusCode::BAD_REQUEST,
            VedaError::QuotaExceeded(_) => StatusCode::TOO_MANY_REQUESTS,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, Json(ApiResponse::<()>::err(self.0.to_string()))).into_response()
    }
}

impl<E: Into<VedaError>> From<E> for AppError { ... }
```

Handler 签名变为：

```rust
async fn write_file(...) -> Result<Json<ApiResponse<FileInfo>>, AppError> {
    let file = fs_service.write(workspace_id, path, content).await?;
    Ok(Json(ApiResponse::ok(file)))
}
```

---

## 9. API 设计

### 9.1 管控面


| Method | Path                        | Auth    | 说明               |
| ------ | --------------------------- | ------- | ---------------- |
| POST   | `/v1/accounts`              | 无       | 注册               |
| POST   | `/v1/accounts/login`        | 无       | 登录，返回 API Key    |
| POST   | `/v1/workspaces`            | API Key | 创建 workspace     |
| GET    | `/v1/workspaces`            | API Key | 列出 workspace     |
| DELETE | `/v1/workspaces/{id}`       | API Key | 删除 workspace     |
| POST   | `/v1/workspaces/{id}/keys`  | API Key | 创建 Workspace Key |
| POST   | `/v1/workspaces/{id}/token` | API Key | 换取短期 JWT         |


### 9.2 数据面


| Method | Path                             | Auth   | 说明            |
| ------ | -------------------------------- | ------ | ------------- |
| PUT    | `/v1/fs/{path}`                  | JWT/WK | 写文件           |
| GET    | `/v1/fs/{path}`                  | JWT/WK | 读文件           |
| GET    | `/v1/fs/{path}?lines=N:M`        | JWT/WK | 按行读取          |
| GET    | `/v1/fs/{path}?list`             | JWT/WK | 列目录           |
| HEAD   | `/v1/fs/{path}`                  | JWT/WK | 文件 stat       |
| DELETE | `/v1/fs/{path}`                  | JWT/WK | 删除            |
| POST   | `/v1/fs/{path}?copy&to={dest}`   | JWT/WK | 复制            |
| POST   | `/v1/fs/{path}?rename&to={dest}` | JWT/WK | 移动/重命名        |
| POST   | `/v1/search`                     | JWT/WK | 搜索            |
| POST   | `/v1/collections`                | JWT/WK | 创建 collection |
| GET    | `/v1/collections`                | JWT/WK | 列出 collection |
| POST   | `/v1/collections/{name}/rows`    | JWT/WK | 插入行           |
| POST   | `/v1/collections/{name}/search`  | JWT/WK | 搜索 collection |
| POST   | `/v1/sql`                        | JWT/WK | 执行 SQL        |
| WS     | `/v1/ws/events`                  | JWT/WK | 实时文件事件        |


---

## 10. 搜索设计

### 10.1 三种模式


| 模式         | 实现                      | 适用场景  |
| ---------- | ----------------------- | ----- |
| hybrid（默认） | dense + sparse，RRF 融合   | 通用    |
| semantic   | dense cosine similarity | 语义相似  |
| fulltext   | BM25 sparse             | 精确关键词 |


### 10.2 Hybrid Search RRF 融合

```
score(doc) = Σ 1 / (k + rank_in_retriever_i)
```

默认 `k = 60`，两路结果合并去重后按 RRF score 排序。

### 10.3 Fallback 机制

Milvus 2.4 以下不支持 BM25 sparse：

- hybrid → 退化为 semantic
- fulltext → 返回错误提示升级 Milvus

---

## 11. Worker 设计

### 11.1 Outbox 消费

```
loop {
    let tasks = task_queue.claim(batch_size).await?;
    if tasks.is_empty() {
        sleep(poll_interval).await;
        continue;
    }
    for task in tasks {
        match task.event_type {
            ChunkSync => {
                let file = metadata_store.get_file(task.file_id).await?;
                let chunks = chunker.chunk(file.content).await?;
                let embeddings = embedding.embed(chunks.texts()).await?;
                vector_store.upsert_chunks(chunks.with_embeddings(embeddings)).await?;
                task_queue.complete(task.id).await?;
            }
            ChunkDelete => {
                vector_store.delete_chunks(task.workspace_id, task.file_id).await?;
                task_queue.complete(task.id).await?;
            }
            CollectionSync => { ... }
        }
    }
}
```

### 11.2 Graceful Shutdown

```rust
let cancel = CancellationToken::new();

tokio::select! {
    _ = worker_loop(cancel.clone()) => {}
    _ = cancel.cancelled() => {
        // drain in-flight tasks
    }
}
```

### 11.3 Reconciler

每 5 分钟执行：

1. 重置超时的 `processing` 任务
2. 标记 `dead` letter（retry >= max_retries）
3. MySQL ↔ Milvus 数据对比，补发缺失的 outbox 事件

---

## 12. 验收指标

### 12.1 功能验收


| 能力                    | 验收标准                                      |
| --------------------- | ----------------------------------------- |
| 文件 CRUD               | cp/cat/ls/mv/rm/mkdir 全部工作                |
| 搜索                    | hybrid/semantic/fulltext 三种模式             |
| Structured Collection | 创建/插入/搜索/SQL 查询                           |
| Raw Vector            | 插入/搜索自定义向量                                |
| 多租户                   | workspace 间完全隔离，不可跨 workspace 访问          |
| 认证                    | API Key + Workspace Token + Workspace Key |
| 大文件                   | >256KB 文件正确分块存储和读取                        |
| 行号读取                  | `?lines=N:M` 正确返回指定行                      |
| 去重                    | 相同内容不重复写入                                 |
| WebSocket             | 文件变更实时推送                                  |


### 12.2 性能验收


| 指标                | 目标                          |
| ----------------- | --------------------------- |
| 文件写入（<256KB）      | p99 < 50ms                  |
| 文件读取（<256KB）      | p99 < 20ms                  |
| 搜索（10万文件）         | p99 < 500ms                 |
| Outbox 处理延迟       | 平均 < 30s（取决于 embedding API） |
| Collection INSERT | p99 < 2s（含同步 embedding）     |


### 12.3 可靠性验收


| 场景                 | 预期行为                 |
| ------------------ | -------------------- |
| Worker crash       | Outbox 任务不丢失，重启后继续处理 |
| Embedding API 超时   | 重试 + 指数退避，不阻塞其他任务    |
| MySQL ↔ Milvus 不一致 | Reconciler 5分钟内自愈    |
| Milvus 短暂不可用       | Outbox 缓冲写入，恢复后补追    |


---

## 13. 部署架构

```
Kubernetes Cluster
├── veda-server (Deployment, 2+ replicas)
│   ├── Axum HTTP server
│   ├── Embedded Worker (可配置禁用)
│   └── Prometheus /metrics
│
├── MySQL 8.0 (StatefulSet or RDS)
│   └── veda database
│
├── Milvus 2.4+ (Helm chart)
│   ├── veda_chunks
│   ├── veda_coll_*
│   └── veda_vectors_*
│
└── Embedding API (外部，如 OpenAI)
```

配置示例：

```toml
[server]
listen_addr = "0.0.0.0:9009"
jwt_secret = "change-me"
worker_enabled = true           # false 可禁用 embedded worker

[mysql]
database_url = "mysql://root:password@mysql:3306/veda"

[milvus]
url = "http://milvus:19530"

[embedding]
api_url = "https://api.openai.com/v1/embeddings"
api_key = "sk-xxx"
model = "text-embedding-3-small"
dimension = 1024

[worker]
poll_interval_ms = 1000
batch_size = 10
reconcile_interval_secs = 300
```

