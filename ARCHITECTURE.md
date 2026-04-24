# Architecture

> 系统现状。实现了什么，什么还没做。每次改架构后更新此文件。

---

## 现状：Phase 1-7 已完成

基础层、存储层、Pipeline、HTTP 层、SQL 引擎、CLI 全部实现，含单元测试和集成测试。Phase 7 新增 Tiered Context Loading（三层信息模型）。

---

## 模块结构

```
veda-types      领域类型、错误定义、API DTO         (已实现)
veda-core       trait 定义 + 业务逻辑               (已实现)
veda-store      MySQL + Milvus trait 实现           (已实现)
veda-pipeline   embedding、chunking、提取、LLM 摘要   (已实现)
veda-sql        DataFusion SQL 引擎                 (已实现)
veda-server     Axum HTTP 层                        (已实现)
veda-cli        CLI 客户端                          (已实现)
veda-fuse       FUSE 挂载（不在默认 workspace）      (已实现)
```

## 已实现能力

- `veda-types`：VedaError enum、ApiResponse、所有领域类型（Account/Workspace/Dentry/FileRecord/FileChunk/OutboxEvent/CollectionSchema/SearchHit/FileSummary/SummaryStatus/DetailLevel 等）、API DTOs、13 个单元测试
- `veda-core`：MetadataStore/MetadataTx/VectorStore/TaskQueue/EmbeddingService/LlmService/AuthStore/CollectionMetaStore/CollectionVectorStore trait 定义；FsService（write/read/list/stat/delete/copy/rename/mkdir/append/glob_files/list_dir_recursive，含 dedup/COW/分层存储/outbox）；SearchService（hybrid/semantic/fulltext 三模式 + Abstract/Overview/Full 三层分级搜索）；CollectionService（create/list/get/delete/insert_rows/search）；path normalization；SHA256 checksum；glob matching；24 个 mock 单元测试 + 14 个工具单元测试
- `veda-store`：MysqlStore（MetadataStore + MetadataTx + TaskQueue + AuthStore + CollectionMetaStore 实现，含 schema migration，新增 veda_summaries 表）；MilvusStore（VectorStore + CollectionVectorStore 实现，REST v2 client，新增 veda_summaries 集合）；10 个 MySQL 集成测试 + 2 个 Milvus 集成测试
- `veda-pipeline`：EmbeddingProvider（OpenAI 兼容 HTTP）；LlmProvider（OpenAI-compatible chat completions，用于 L0/L1 摘要生成）；semantic_chunk（heading-based + sliding window）；storage_chunk（256KB 边界 + start_line）；extract_text（text/plain 占位）；summary 模块（generate_l0/generate_l1/aggregate_dir_summary + prompt 模板）；6 个 chunking 单元测试 + 3 个 summary 单元测试 + 4 个 embedding 集成测试
- `veda-sql`：VedaSqlEngine（DataFusion session 管理）；FilesTable（递归 dentry 枚举）；CollectionTable（Milvus 查询 → Arrow）；8 个 FS SQL 标量函数（veda_read/write/append/exists/size/mtime/remove/mkdir，友好错误消息）；`embedding()` UDF（文本 → JSON 向量）；`veda_fs()` Table Function（目录列举 / 文件读取 / glob 匹配，CSV/TSV/JSONL/plain text 自动解析）；`search()` UDTF（向量搜索，支持 hybrid/semantic/fulltext 模式 + limit 参数）；`veda_fs_events()` Table Function（事件查询，支持 since_id/path_prefix/limit）；`veda_storage_stats()` Table Function（文件/目录/字节统计）；支持 SELECT/WHERE/COUNT/JOIN 等标准 SQL；37 个 mock 单元测试
- `veda-server`：ServerConfig（TOML 加载，新增 LlmConfig）；JWT 签发/验证；API Key + Workspace Key 认证中间件；Account/Workspace/File/Search/Collection/SQL 全部 REST 路由；Worker（outbox 消费 + chunk sync + summary sync + dir summary sync）；`GET /v1/summary/{path}` 摘要查询端点；搜索 API 支持 `detail_level` 参数；graceful shutdown；4 个 HTTP 端到端集成测试（fs CRUD/stat/mkdir/copy/rename/append/CAS/range/auth）
- `veda-server`：新增 `POST /v1/fs/{path}` append 路由
- `veda-cli`：clap 命令行解析；account create/login；workspace create/list/use；cp/cat/ls/mv/rm/mkdir/append；search（支持 --detail-level abstract/overview/full）；summary（查看文件/目录摘要）；collection CRUD；sql；config 管理；$HOME/.config/veda/config.toml 持久化
- `veda-fuse`：FUSE 文件系统挂载（fuser 0.14）；子命令 mount/umount；daemon 化（fork+setsid，pipe 通知 parent）；blocking HTTP 客户端（stat/read/write/list/delete/mkdir/rename/read_range）；InodeTable（inode ↔ path 双向映射 + 可配置 attr TTL）；WriteHandle（dirty 标记消除 flush+release 双写）；LRU ReadCache（小文件全量缓存，大文件 Range read）；SSE watcher（后台线程连接 /v1/events，远程变更时失效 attr+read cache）；mount 选项（--cache-size/--attr-ttl/--allow-other/--read-only/--debug）；需要 macFUSE（macOS）或 libfuse（Linux）
- `veda-server`：新增 `GET /v1/events` SSE 端点（轮询 veda_fs_events 表，cursor-based）；`GET /v1/fs/{path}` 支持 `Range` header 返回 206 Partial Content

## 测试策略

- 单元测试：`cargo test`（128 个测试，全部自动运行）
- 集成测试：`cargo test -- --ignored`（17 个测试，需要 `config/test.toml` 配置真实 MySQL/Milvus/Embedding 服务）
- 敏感配置：`config/test.toml`（已 gitignore），模板见 `config/test.toml.example`

## 待实现

- Reconciler（MySQL ↔ Milvus 数据对比自愈）
- Prometheus metrics 导出
- PDF text extraction / Image OCR
- CLI/FUSE 端到端测试、Docker Compose、K8s Helm chart

## 关键设计决策

详见 `docs/design.md`。摘要：

- MySQL = control plane (元数据、认证、outbox)
- Milvus = data plane (向量搜索、structured collection 数据)
- 文件分层存储：≤256KB inline，>256KB chunked
- Content-addressed dedup (SHA256)
- Outbox pattern 实现最终一致性 + reconciler 自愈
- Account → Workspace 多租户，API Key + Workspace Token 认证
- VedaError::Storage 使用 String（而非 anyhow::Error）避免 lib crate 兼容问题
- **三层信息模型 (Tiered Context Loading)**：
  - L0 Abstract (~100 tokens)：文件/目录的一句话摘要，存入 Milvus 做向量搜索
  - L1 Overview (~2k tokens，可通过 `max_summary_tokens` 配置)：结构化概览，按需从 MySQL 批量加载
  - L2 Full (原文 chunk)：现有 chunk 搜索
  - 写入流程：ChunkSync → SummarySync (LLM 并行生成 L0+L1) → DirSummarySync (自底向上聚合，含去重防抖)
  - 文件和目录的 L0 均写入 Milvus `veda_summaries` 集合（Abstract 搜索可命中目录）
  - SummarySync / DirSummarySync 入队前检查去重，避免重复 LLM 调用
  - 搜索 API：`detail_level` 参数控制返回粒度，Abstract/Overview/Full 三级
  - LLM 配置可选，未配置时 summary 功能自动禁用

