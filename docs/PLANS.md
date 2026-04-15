# Veda 实施计划

---

## Phase 0: 项目脚手架 ✅

- [x] Cargo workspace 初始化（8 crates）
- [x] AGENTS.md / README.md
- [x] 设计文档 (`docs/design.md`)

---

## Phase 1: 基础层（veda-types + veda-core）✅

> 目标：定义所有领域类型、trait 接口、错误体系、路径工具。完成后可以写 mock 测试。

- [x] `veda-types`: VedaError enum + ApiResponse
- [x] `veda-types`: API request/response DTOs (serde)
- [x] `veda-types`: 领域类型 (Account, Workspace, Dentry, FileRecord, FileChunk, OutboxEvent, CollectionSchema)
- [x] `veda-core`: MetadataStore + MetadataTx trait
- [x] `veda-core`: VectorStore trait
- [x] `veda-core`: TaskQueue trait
- [x] `veda-core`: EmbeddingService trait
- [x] `veda-core`: FsService 业务逻辑（write/read/list/delete/copy/rename/mkdir）
- [x] `veda-core`: SearchService 业务逻辑
- [ ] `veda-core`: CollectionService 业务逻辑
- [x] `veda-core`: path normalization + validation
- [x] `veda-core`: SHA256 checksum utility
- [x] 单元测试：用 mock trait 测试 FsService（去重、COW、分层存储）

---

## Phase 2: 存储层（veda-store）✅

> 目标：实现 MySQL 和 Milvus 的 trait 实现。完成后可以跑集成测试。

- [x] MySQL schema migration（所有表）
- [x] `veda-store/mysql`: MetadataStore 实现
- [x] `veda-store/mysql`: MetadataTx 实现
- [x] `veda-store/mysql`: TaskQueue 实现
- [ ] `veda-store/mysql`: Account/Workspace/Key CRUD
- [x] `veda-store/milvus`: Milvus REST v2 client
- [x] `veda-store/milvus`: VectorStore 实现
- [ ] `veda-store/milvus`: Collection 动态 schema 管理
- [x] 集成测试：真实 MySQL + Milvus（config/test.toml 驱动）

---

## Phase 3: Pipeline（veda-pipeline）✅

> 目标：embedding、chunking、内容提取。

- [x] OpenAI-compatible embedding provider
- [x] Semantic chunking（heading-based + sliding window）
- [x] Storage chunking（256KB 边界 + start_line 计算）
- [ ] PDF text extraction
- [ ] Image OCR (可后续)
- [x] 单元测试：chunking 算法、line count
- [x] 集成测试：embedding API 真实调用

---

## Phase 4: HTTP 层（veda-server）

> 目标：Axum 路由、认证中间件、WebSocket。

- [ ] Config 加载（TOML + env override）
- [ ] JWT 签发 / 验证中间件
- [ ] API Key / Workspace Key 验证中间件
- [ ] Account routes（注册、登录）
- [ ] Workspace routes（CRUD、key 管理、token 换取）
- [ ] File routes（PUT/GET/HEAD/DELETE/COPY/RENAME）
- [ ] Search route
- [ ] Collection routes（CRUD、insert、search）
- [ ] SQL route
- [ ] WebSocket events endpoint
- [ ] Worker 启动 + graceful shutdown
- [ ] Reconciler
- [ ] Prometheus metrics

---

## Phase 5: SQL 引擎（veda-sql）

> 目标：DataFusion 集成，支持 SQL 查询文件和 collection。

- [ ] DataFusion session 管理
- [ ] File table provider（从 MySQL 加载）
- [ ] Collection table provider（从 Milvus 加载）
- [ ] `embedding()` UDF
- [ ] `search()` UDTF

---

## Phase 6: CLI（veda-cli）

> 目标：完整的命令行客户端。

- [ ] account create / login
- [ ] workspace create / list / use
- [ ] cp / cat / ls / mv / rm / mkdir
- [ ] search
- [ ] collection create / insert / search
- [ ] sql
- [ ] config show / set
- [ ] status

---

## Phase 7: 稳定化

- [ ] 端到端测试
- [ ] 文档完善
- [ ] Docker Compose 开发环境
- [ ] K8s Helm chart
- [ ] CI/CD pipeline
