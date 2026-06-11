# db workspace（向量服务）接业务方前待办

> 来源：2026-05-29 db workspace deep review。
> 当场已修并通过编译：**C1**（db search/query 加 Strong consistency，修复 read-your-writes）、**H2**（provisioning workspace+dataset 合并为单事务 `create_db_workspace`）、**L2**（加 `DEFAULT_CATEGORY` 常量）、**L3**（filter `in` 数组 ≤100 上限）。
> 暂不处理：L1（delete_count 是 tombstone 数，文档已诚实标注）。
> 下列 3 项 Joe 决定写入待办。

## H1. 软删资源的 Milvus GC（存储泄漏 + 删除路径不一致）

- **现状**：`delete_workspace`（mysql.rs:2333）/ `archive_dataset`（mysql.rs:2434）软删只改 MySQL `status='archived'`，不动 Milvus collection / dataset 内向量行。
- **隔离 OK**：`load_db_workspace`（auth.rs:183）和 `get_active_dataset_by_name` 会把 archived 资源挡成 404，无越权——这不是安全漏洞。
- **真问题**：① 正常删除走软删，而 provisioning **回滚**走 `hard_delete + drop_collection`，两条路径行为不一致；② 软删后**永远没有 GC** 清 Milvus collection / dataset 向量 → 存储无限泄漏。
- **待办**：加一个 admin GC job，扫 archived workspace/dataset → drop 对应 Milvus collection / 清 dataset 向量。文档 `vectors-merge-plan.md` §10 已规划为 v1 admin endpoint。
- **触发**：接外部业务方、workspace/dataset 频繁增删。alpha 量小不阻塞。

## M1. 启动校验 Milvus 维度（fs/db 两条路径一个 helper 覆盖）

- **现状**：`create_vector_collection`（milvus.rs，吞 `CollectionAlreadyExists` 后直接复用）和 fs 路径 `init_collections`（milvus.rs:1202，只 `collection_exists → create`）**都不 describe 已有 collection 的 dim 与 config 比对**。
- **问题**：换 embedding model / 改 `embedding.dimension` / 复用孤儿 collection 时维度静默错配，报错点远离根因（upsert 时 Milvus 才拒，难排查）。
- **待办**：启动 / provision 时 describe collection，dim 与 config 不一致则明确报错退出（而非静默复用）。一处 helper 同时覆盖 fs 的 `veda_chunks` / `veda_summaries` 和 db 的 `ws_*_default`。
- **范围**：正常单部署路径安全（embed 维度已在 embedding.rs 校验），仅 config 变更 / 孤儿复用这条运维边缘触发。

## A1. collection 内存天花板（架构扩展性硬门槛）

- **现状**：每个 db workspace 一个 Milvus collection，provision 时即 `load_collection`，**永不 unload**（文档 §决策4）。
- **问题**：plan 目标 1500 workspace = 1500 个常驻内存 collection。Milvus loaded collection 数受 querynode 内存 + 元数据限制，到量级后 `load` 失败 → 新 workspace provision 失败（并触发 H2 回滚）。这是这套架构最硬的扩展性天花板。
- **待办（接公司业务方前硬门槛）**：① spike 验证目标 Milvus 部署的 loaded collection 上限 + 单 collection 内存占用；② 设计 lazy load + LRU unload（文档 §决策4 已列为 v1）。
- 风险已记在 `vectors-merge-plan.md` §7。

## D1. 对外 SDK（Java/Python）——走 OpenAPI 生成，不上 gRPC

> **状态（2026-06-10）：Java 已交付，路线变更。** Java SDK 按**手写**路线发布
> 0.0.1-SNAPSHOT（`5622111`，方案见 `docs/archive/plans/java-sdk-db-plan.md`，明确否决了
> 本条的 generator 倾向）；OpenAPI spec + openapi-generator 路线作废。Python SDK 待真实需求再起。
> 下文保留作"为什么不上 gRPC"的决策记录。

- **背景**：业务方接入想要 Java/Python SDK。评估结论：**不改 gRPC**——现有 axum REST 已适合做 SDK，瓶颈在 embedding+Milvus 而非 JSON 序列化；gRPC 改造（tonic + .proto + handler/auth/error 全重写 + CLI/FUSE/web/部署全改）成本高且负收益。SDK 友好度取决于**有没有机器可读契约（OpenAPI）**，与协议无关。
- **现状**：REST/JSON over axum；`veda-types/src/api.rs` 已是强类型 Request/Response；统一 `ApiResponse<T>` + 稳定 `error_code`；cursor 分页已具备。**无 OpenAPI spec**（代码注释引用过 vss `openapi.yaml`）。
- **待办**：① 产出 OpenAPI spec——对齐迁移 vss `openapi.yaml`，或给 axum 加 `utoipa` 从代码生成，覆盖 vectors 数据面 + accounts/workspaces/datasets/admin tokens 控制面；② `openapi-generator` 生成 Java/Python client 骨架；③ 薄封装：auth 注入、重试、cursor 分页迭代器。
- **不做**：不替换对外接口为 gRPC。若将来出现高 QPS 服务间内部调用（profile 证明 JSON 是瓶颈）或流式需求，再考虑**对外 REST + 对内 gRPC 双协议**，而非换掉业务方 SDK 接口。
- **触发**：正式对业务方发 SDK 时。alpha 自用 / curl 直连不阻塞。
- **关联**：完整接口参考见 `docs/api/db-workspace-api.md`。

## S1. Scoped `vk_` 越权签发 out-of-scope `wk_`（控制面 scope 未 enforce）

> **状态（2026-06-09）：✅ 已修**（`6e6d4bf`）。scope 校验下沉到唯一 ownership 闸
> `load_owned_workspace`（`check_workspace_allowed`），所有带 `ws_id` 的控制面路由
> （含 key create/list/delete）自动继承，scoped `vk_` 不再能越权签发 `wk_`。下文保留原始分析。

- **来源**：2026-06-04 Codex review（A 迁移第一步 app_id 自动开通 review 时顺带发现，**非该次引入**，是 `fa7f91c` 引入 key 端点时的既有遗漏）。
- **现状**：workspace key 端点 create / list / delete（`account.rs:507` / `:556` / `:570`）只调 `load_owned_workspace`（仅校验 workspace 归属本账号），**不校验** token 的 `allowed_workspaces` scope；而 dataset 路径 `load_db_workspace`（`auth.rs:152`）已正确 `check_workspace_allowed`（scope 定义见 `auth.rs:17`）。
- **真问题**：一个 `allowed_workspaces` 限定到 `ws1` 的 scoped `vk_`，可对 `ws2` 调 `POST /v1/workspaces/{ws2}/keys` 拿到 `ws2` 的 readwrite `wk_`，绕过 scope 拿到越权数据面凭证。dataset 与 key 两条路径行为不一致。
- **修法**（约 3 行 × 3 处）：key 创建 / 列表 / 删除在 `load_owned_workspace` 后补 `auth.check_workspace_allowed(&ws_id)?`，对齐 dataset 路径。
- **决策（2026-06-04 Joe）**：v0 暂不修，记入待办。A 迁移用 `app_id` 控制面（鉴权外移）取代 `vk_` 控制面后，scoped `vk_` 这条路径随之消失、问题自然消解；若决定让 `vk_` 控制面长期并存，则必须补这个校验再对外。
- **关联**：`docs/archive/plans/platform-admin-api-plan.md` §7（"`vk_` 仍是账号根权限、无能力分级"风险的具体实例）。

---

## 并入：`vectors-merge-backlog` 归档时仍 open 的尾巴（2026-06-10）

> 来源：`docs/archive/vectors-merge-backlog.md`（Stage 4/5 已完成归档，下列三条是没兑现的承诺）。

### I6 残留 — EmbeddingCache 无 metrics

backlog 承诺 Stage 4 上线前补 `embed_cache_hits` / `embed_cache_misses` 两个 metric。
`veda_embed_latency_seconds` / `veda_embed_total` 已有（embedding.rs），但 cache 命中率 /
size / eviction 至今零可见。embedding 限流是当前已知最硬瓶颈（QPM 天花板），cache 命中率
是评估"攒批 executor"方案收益的直接输入——做 embedding 吞吐优化（`embedding-throughput-plan.md`）时一并补。

### delete-by-pk 500 上限缺真实 Milvus 边界测试

`MAX_PK_BATCH=500` 已实现（vectors.rs），但"500 个 PK × ~128 字节 ≈ 65KB filter 字符串、
Milvus 2.6 REST 是否接受"从未实测。补一个 500-pk delete 集成测试；失败则下调上限。

### vectors-merge-plan 回写清单（6 条，低优先级）

backlog 列的 plan 文档 drift 一条都没回写：§1.1 `ENUM('fs','db')`→`VARCHAR(16)`、
`expires_at TIMESTAMP`→`DATETIME`、§4 `try_get_with`→manual batch-preserving lookup、
§10 dataset DELETE 标已实现、Stage 1 表格删 admin token 占位、§6.1 补 AuthWorkspace 隐式
enforce 注记。改 `docs/vectors-merge-plan.md` 时顺手清掉。
