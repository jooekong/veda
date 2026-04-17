# Architecture

> 系统现状。实现了什么，什么还没做。每次改架构后更新此文件。

---

## 现状：Phase 1-6 已完成

基础层、存储层、Pipeline、HTTP 层、SQL 引擎、CLI 全部实现，含单元测试和集成测试。

---

## 模块结构

```
veda-types      领域类型、错误定义、API DTO         (已实现)
veda-core       trait 定义 + 业务逻辑               (已实现)
veda-store      MySQL + Milvus trait 实现           (已实现)
veda-pipeline   embedding、chunking、提取           (已实现)
veda-sql        DataFusion SQL 引擎                 (已实现)
veda-server     Axum HTTP 层                        (已实现)
veda-cli        CLI 客户端                          (已实现)
veda-fuse       FUSE 挂载（不在默认 workspace）      (空壳)
```

## 已实现能力

- `veda-types`：VedaError enum、ApiResponse、所有领域类型（Account/Workspace/Dentry/FileRecord/FileChunk/OutboxEvent/CollectionSchema/SearchHit 等）、API DTOs、13 个单元测试
- `veda-core`：MetadataStore/MetadataTx/VectorStore/TaskQueue/EmbeddingService/AuthStore/CollectionMetaStore/CollectionVectorStore trait 定义；FsService（write/read/list/stat/delete/copy/rename/mkdir，含 dedup/COW/分层存储/outbox）；SearchService（hybrid/semantic/fulltext 三模式）；CollectionService（create/list/get/delete/insert_rows/search）；path normalization；SHA256 checksum；21 个 mock 单元测试 + 12 个工具单元测试
- `veda-store`：MysqlStore（MetadataStore + MetadataTx + TaskQueue + AuthStore + CollectionMetaStore 实现，含 schema migration）；MilvusStore（VectorStore + CollectionVectorStore 实现，REST v2 client）；10 个 MySQL 集成测试 + 2 个 Milvus 集成测试
- `veda-pipeline`：EmbeddingProvider（OpenAI 兼容 HTTP）；semantic_chunk（heading-based + sliding window）；storage_chunk（256KB 边界 + start_line）；extract_text（text/plain 占位）；6 个 chunking 单元测试 + 4 个 embedding 集成测试
- `veda-sql`：VedaSqlEngine（DataFusion session 管理）；FilesTable（递归 dentry 枚举）；CollectionTable（Milvus 查询 → Arrow）；8 个 FS SQL 标量函数（veda_read/write/append/exists/size/mtime/remove/mkdir）；支持 SELECT/WHERE/COUNT/JOIN 等标准 SQL；13 个 mock 单元测试
- `veda-server`：ServerConfig（TOML 加载）；JWT 签发/验证；API Key + Workspace Key 认证中间件；Account/Workspace/File/Search/Collection/SQL 全部 REST 路由；Worker（outbox 消费 + chunk sync）；graceful shutdown；1 个 HTTP 集成测试
- `veda-cli`：clap 命令行解析；account create/login；workspace create/list/use；cp/cat/ls/mv/rm/mkdir；search；collection CRUD；sql；config 管理；$HOME/.config/veda/config.toml 持久化

## 测试策略

- 单元测试：`cargo test`（65 个测试，全部自动运行）
- 集成测试：`cargo test -- --ignored`（17 个测试，需要 `config/test.toml` 配置真实 MySQL/Milvus/Embedding 服务）
- 敏感配置：`config/test.toml`（已 gitignore），模板见 `config/test.toml.example`

## 待实现

- `veda_fs()` Table Function（目录列举 / 文件读取为行 / glob 展开 / CSV·JSONL 自动解析）
- `veda_fs_events()` / `veda_storage_stats()` Table Function
- `POST /v1/fs/{path}?append` HTTP API
- WebSocket 实时事件推送
- Reconciler（MySQL ↔ Milvus 数据对比自愈）
- Prometheus metrics 导出
- `embedding()` UDF / `search()` UDTF
- PDF text extraction / Image OCR
- veda-fuse（FUSE 挂载）
- 端到端测试、Docker Compose、K8s Helm chart

## 关键设计决策

详见 `docs/design.md`。摘要：

- MySQL = control plane (元数据、认证、outbox)
- Milvus = data plane (向量搜索、structured collection 数据)
- 文件分层存储：≤256KB inline，>256KB chunked
- Content-addressed dedup (SHA256)
- Outbox pattern 实现最终一致性 + reconciler 自愈
- Account → Workspace 多租户，API Key + Workspace Token 认证
- VedaError::Storage 使用 String（而非 anyhow::Error）避免 lib crate 兼容问题

