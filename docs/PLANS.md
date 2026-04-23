# Veda 实施计划

---

## Phase 0: 项目脚手架 ✅

- Cargo workspace 初始化（8 crates）
- AGENTS.md / README.md
- 设计文档 (`docs/design.md`)

---

## Phase 1: 基础层（veda-types + veda-core）✅

> 目标：定义所有领域类型、trait 接口、错误体系、路径工具。完成后可以写 mock 测试。

- `veda-types`: VedaError enum + ApiResponse
- `veda-types`: API request/response DTOs (serde)
- `veda-types`: 领域类型 (Account, Workspace, Dentry, FileRecord, FileChunk, OutboxEvent, CollectionSchema)
- `veda-core`: MetadataStore + MetadataTx trait
- `veda-core`: VectorStore trait
- `veda-core`: TaskQueue trait
- `veda-core`: EmbeddingService trait
- `veda-core`: FsService 业务逻辑（write/read/list/delete/copy/rename/mkdir）
- `veda-core`: SearchService 业务逻辑
- `veda-core`: CollectionService 业务逻辑
- `veda-core`: path normalization + validation
- `veda-core`: SHA256 checksum utility
- 单元测试：用 mock trait 测试 FsService（去重、COW、分层存储）

---

## Phase 2: 存储层（veda-store）✅

> 目标：实现 MySQL 和 Milvus 的 trait 实现。完成后可以跑集成测试。

- MySQL schema migration（所有表）
- `veda-store/mysql`: MetadataStore 实现
- `veda-store/mysql`: MetadataTx 实现
- `veda-store/mysql`: TaskQueue 实现
- `veda-store/mysql`: Account/Workspace/Key CRUD
- `veda-store/milvus`: Milvus REST v2 client
- `veda-store/milvus`: VectorStore 实现
- `veda-store/milvus`: Collection 动态 schema 管理
- 集成测试：真实 MySQL + Milvus（config/test.toml 驱动）

---

## Phase 3: Pipeline（veda-pipeline）✅

> 目标：embedding、chunking、内容提取。

- OpenAI-compatible embedding provider
- Semantic chunking（heading-based + sliding window）
- Storage chunking（256KB 边界 + start_line 计算）
- PDF text extraction
- Image OCR (可后续)
- 单元测试：chunking 算法、line count
- 集成测试：embedding API 真实调用

---

## Phase 4: HTTP 层（veda-server）✅

> 目标：Axum 路由、认证中间件、WebSocket。

- Config 加载（TOML + env override）
- JWT 签发 / 验证中间件
- API Key / Workspace Key 验证中间件
- Account routes（注册、登录）
- Workspace routes（CRUD、key 管理、token 换取）
- File routes（PUT/GET/HEAD/DELETE/COPY/RENAME）
- Search route
- Collection routes（CRUD、insert、search）
- SQL route
- WebSocket events endpoint
- Worker 启动 + graceful shutdown
- Reconciler
- Prometheus metrics

---

## Phase 5: SQL 引擎（veda-sql）✅

> 目标：DataFusion 集成，支持 SQL 查询文件和 collection。

- DataFusion session 管理
- File table provider（从 MySQL 加载）
- Collection table provider（从 Milvus 加载）
- `embedding()` UDF（文本 → JSON 向量字符串）
- `search()` UDTF（hybrid/semantic/fulltext，返回 file_id/chunk_index/content/score/path）

---

## Phase 6: CLI（veda-cli）✅

> 目标：完整的命令行客户端。

- account create / login
- workspace create / list / use
- cp / cat / ls / mv / rm / mkdir
- search
- collection create / insert / search
- sql
- config show / set

---

## Phase 7: FUSE 挂载（veda-fuse）✅

> 目标：将 Veda workspace 挂载为本地文件系统。

- Cargo.toml + clap CLI 入口
- blocking HTTP client（stat/read/write/list/delete/mkdir/rename）
- InodeTable（inode ↔ path 双向映射 + attr TTL 缓存）
- fuser::Filesystem 实现（lookup/getattr/readdir/read/write/create/mkdir/unlink/rmdir/rename/setattr/flush/release）
- 写缓冲机制（open → buffer → flush/release 时 PUT）
- 子命令架构（mount/umount）
- 后台挂载（fork+setsid daemon 化，pipe 通知 parent 就绪）
- umount 命令（macOS umount / Linux fusermount3）
- 消除 flush+release 双写（dirty flag）
- mount 选项（--cache-size/--attr-ttl/--allow-other/--read-only/--debug/--foreground）
- read-only 模式（写操作返回 EROFS）
- LRU 读缓存（小文件全量缓存，大文件 Range read）
- server 端 Range header 支持（206 Partial Content）
- server 端 SSE 事件端点（GET /v1/events）
- FUSE 端 SSE watcher（后台线程，远程变更时失效 attr+read cache）
- FsService.query_events / read_file_range 新 API

---

## Phase 8: 稳定化

- 端到端测试（Server FS API：CRUD/stat/mkdir/copy/rename/append/CAS/range/auth）
- 端到端测试（CLI/FUSE smoke）
- 文档完善
- Docker Compose 开发环境
- K8s Helm chart
- CI/CD pipeline