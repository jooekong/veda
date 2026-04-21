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
veda-fuse       FUSE 挂载（不在默认 workspace）      (已实现)
```

## 已实现能力

- `veda-types`：VedaError enum、ApiResponse、所有领域类型（Account/Workspace/Dentry/FileRecord/FileChunk/OutboxEvent/CollectionSchema/SearchHit 等）、API DTOs、13 个单元测试
- `veda-core`：MetadataStore/MetadataTx/VectorStore/TaskQueue/EmbeddingService/AuthStore/CollectionMetaStore/CollectionVectorStore trait 定义；FsService（write/read/list/stat/delete/copy/rename/mkdir/append/glob_files/list_dir_recursive，含 dedup/COW/分层存储/outbox）；SearchService（hybrid/semantic/fulltext 三模式）；CollectionService（create/list/get/delete/insert_rows/search）；path normalization；SHA256 checksum；glob matching；21 个 mock 单元测试 + 14 个工具单元测试
- `veda-store`：MysqlStore（MetadataStore + MetadataTx + TaskQueue + AuthStore + CollectionMetaStore 实现，含 schema migration）；MilvusStore（VectorStore + CollectionVectorStore 实现，REST v2 client）；10 个 MySQL 集成测试 + 2 个 Milvus 集成测试
- `veda-pipeline`：EmbeddingProvider（OpenAI 兼容 HTTP）；semantic_chunk（heading-based + sliding window）；storage_chunk（256KB 边界 + start_line）；extract_text（text/plain 占位）；6 个 chunking 单元测试 + 4 个 embedding 集成测试
- `veda-sql`：VedaSqlEngine（DataFusion session 管理）；FilesTable（递归 dentry 枚举）；CollectionTable（Milvus 查询 → Arrow）；8 个 FS SQL 标量函数（veda_read/write/append/exists/size/mtime/remove/mkdir，友好错误消息）；`embedding()` UDF（文本 → JSON 向量）；`veda_fs()` Table Function（目录列举 / 文件读取 / glob 匹配，CSV/TSV/JSONL/plain text 自动解析）；`search()` UDTF（向量搜索，支持 hybrid/semantic/fulltext 模式 + limit 参数）；`veda_fs_events()` Table Function（事件查询，支持 since_id/path_prefix/limit）；`veda_storage_stats()` Table Function（文件/目录/字节统计）；支持 SELECT/WHERE/COUNT/JOIN 等标准 SQL；37 个 mock 单元测试
- `veda-server`：ServerConfig（TOML 加载）；JWT 签发/验证；API Key + Workspace Key 认证中间件；Account/Workspace/File/Search/Collection/SQL 全部 REST 路由；Worker（outbox 消费 + chunk sync）；graceful shutdown；1 个 HTTP 集成测试
- `veda-server`：新增 `POST /v1/fs/{path}` append 路由
- `veda-cli`：clap 命令行解析；account create/login；workspace create/list/use；cp/cat/ls/mv/rm/mkdir/append；search；collection CRUD；sql；config 管理；$HOME/.config/veda/config.toml 持久化
- `veda-fuse`：FUSE 文件系统挂载（fuser 0.14）；子命令 mount/umount；daemon 化（fork+setsid，pipe 通知 parent）；blocking HTTP 客户端（stat/read/write/list/delete/mkdir/rename/read_range）；InodeTable（inode ↔ path 双向映射 + 可配置 attr TTL）；WriteHandle（dirty 标记消除 flush+release 双写）；LRU ReadCache（小文件全量缓存，大文件 Range read）；SSE watcher（后台线程连接 /v1/events，远程变更时失效 attr+read cache）；mount 选项（--cache-size/--attr-ttl/--allow-other/--read-only/--debug）；需要 macFUSE（macOS）或 libfuse（Linux）
- `veda-server`：新增 `GET /v1/events` SSE 端点（轮询 veda_fs_events 表，cursor-based）；`GET /v1/fs/{path}` 支持 `Range` header 返回 206 Partial Content

## 测试策略

- 单元测试：`cargo test`（95 个测试，全部自动运行）
- 集成测试：`cargo test -- --ignored`（17 个测试，需要 `config/test.toml` 配置真实 MySQL/Milvus/Embedding 服务）
- 敏感配置：`config/test.toml`（已 gitignore），模板见 `config/test.toml.example`

## 待实现

- Reconciler（MySQL ↔ Milvus 数据对比自愈）
- Prometheus metrics 导出
- PDF text extraction / Image OCR
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

