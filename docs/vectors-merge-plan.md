# Veda 承接公司向量服务 · v2 方案

> 起源：归并 vss (Java/Spring Boot, v3.7 设计稿, pre-code) 到 Veda。
> 决策：vss Java 项目停止；Veda 同时承接个人知识库（fs）与公司向量服务（db）。
> 状态：v2 草稿。
>
> **更新 2026-06-02**：原列为 v1 的 hybrid / fulltext / `score_type` 已实现并接通
> `/v1/vectors/search`（`mode = hybrid|semantic|fulltext`，默认 hybrid）。本文档
> §3.2「search 仅 semantic」、§3.4「allow_fallback（v0 没 hybrid）」、§5 v0/v1 表中
> 的 hybrid/fulltext 行均已过时——以 `docs/api/vectors.md` + `docs/api/db-workspace-api.md`
> + CHANGELOG 为对外契约的现行来源，实现细节见 `docs/plans/db-sparse-vector-plan.md`。

## 0. 决策摘要

- **运行环境**：Milvus **2.6+**（支持 BM25 function + sparse_vector）；MySQL 8.0+。注意：`crates/veda-store/src/milvus.rs:185-189` 注释写的"2.4+"是过时下限，实际部署 2.6+，Stage 0 验证 |
- **workspace 加 kind 字段，二选一锁死**：`fs` (走文件路) 或 `db` (走 Pinecone-like 裸向量)
- **鉴权：token-based，token 上加 `app_id` 字符串属性用于治理**。app 不是安全边界（不限制访问），仅供运维找业务方。安全靠 `token + allowed_workspaces`。不引入 app 实体表，不切 bcrypt（继续 SHA-256），无 grants/permissions tier/per-dataset scope
- **v0 砍到最小**：先跑通流程；限流 / 熔断 / audit / idempotency / 完整 Filter DSL / consistency 三档 / outbox 异步路径 / L2 Redis / dedicated collection 全部推 v1
- **写入走同步 Milvus**，返 commit_ts；不复用 outbox（避免破坏 read-your-writes）
- **embedding 内置**：复用 Veda 既有 `EmbeddingProvider` + 加 `moka::future::Cache` L1
- **每 workspace 一个 Milvus collection**；embedding model 由服务端 `config/server.toml [embedding]` 单一指定，v0 业务方不可选
- **软删除一律用 `status='archived'`**，与既有 `WorkspaceStatus` 枚举（active/archived）对齐

---

## 1. MySQL 建模

### 1.1 扩展现有表

```sql
-- workspace 加 kind + 治理标签
ALTER TABLE veda_workspaces
  ADD COLUMN kind ENUM('fs','db') NOT NULL DEFAULT 'fs',
  ADD COLUMN app_id VARCHAR(64) NULL,             -- 治理标签,标识业务方,与鉴权无关
  ADD INDEX idx_app (app_id);
-- embedding_model / embedding_dim 不存表:统一从 config 读取
-- physical_collection 不存表:运行时按 "ws_<hash8(workspace_id)>" 推导

-- token 加 app + workspace scope
ALTER TABLE veda_api_keys
  ADD COLUMN app_id VARCHAR(64) NULL,             -- 治理标签,运维找业务方用,不是安全边界
  ADD COLUMN allowed_workspaces JSON NULL,        -- ["ws_a","ws_b"] 或 NULL=不限
  ADD COLUMN expires_at TIMESTAMP NULL,
  ADD INDEX idx_app (app_id);
-- key_hash 算法不变 (SHA-256),内部服务无需 bcrypt
```

### 1.2 新增表

```sql
-- Pinecone-style dataset: workspace 内逻辑分组,共享同一 Milvus collection
-- 通过 collection 内 scalar 字段 dataset_id 区分
CREATE TABLE veda_datasets (
  id            VARCHAR(36) PRIMARY KEY,
  workspace_id  VARCHAR(36) NOT NULL,
  name          VARCHAR(64) NOT NULL,            -- e.g. "products" / "faq"
  status        VARCHAR(16) DEFAULT 'active',
  created_at    TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  updated_at    TIMESTAMP DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
  UNIQUE INDEX idx_ws_name (workspace_id, name),
  INDEX idx_workspace (workspace_id)
);
```

### 1.3 v0 不引入的表（记录留痕）

| 表 | 不做的原因 |
|---|---|
| `veda_apps` | app 只是 token 上的字符串属性，不需要实体表 |
| `veda_grants` | 跨 app 共享 v0 无需求 |
| `veda_idempotency_keys` | 写不重试 v0 没问题 |
| `veda_audit_log` | 结构化日志先承担，DB 表 v1 再加 |
| `veda_filterable_fields` | Filter DSL v0 砍到 must + eq/in/range，path index 不需要 |

### 1.4 老表保留不动

- `veda_dentries / veda_files / veda_file_contents / veda_file_chunks` — fs kind workspace 专属
- `veda_summaries` — fs 三层摘要专属
- `veda_collection_schemas` — Veda structured collections，与 datasets 正交（dataset 是 Pinecone-style 裸向量，collection_schemas 是 schema-defined 结构化）
- `veda_outbox / veda_fs_events` — fs 异步事件专属，**vector 写入不复用**
- `veda_accounts / veda_api_keys / veda_workspaces / veda_workspace_keys` — 复用并扩展

### 1.5 鉴权流程（v0）

```
1. HTTP header `Authorization: Bearer <token>`
2. SHA-256(token) → 查 veda_api_keys.key_hash → 拿到 row
3. 校验 status='active' AND (expires_at IS NULL OR expires_at > now())
4. 解析请求体 workspace_id (vector API) 或 path (file API)
5. 查 veda_workspaces by workspace_id,校验 status='active'
6. 校验 workspace.kind matches API kind:
     - fs API (`/v1/files/*`, `/v1/abstract/*`, `/v1/overview/*`, `/v1/grep`, `/v1/search`, `/v1/sql`) → 要求 kind='fs'
     - vector API (`/v1/vectors/*`, `/v1/workspaces/{ws}/datasets`) → 要求 kind='db'
     不匹配 → 400 WORKSPACE_KIND_MISMATCH
7. 校验 token.allowed_workspaces:
     - IS NULL → 不限,通过
     - IS NOT NULL → 要求 workspace_id ∈ allowed_workspaces,否则 403 FORBIDDEN
8. 进入业务 handler
```

**app_id 不参与上述校验**——它只是 token / workspace 上的治理标签，audit log（v1）会记录调用者 app_id 供运维追溯。

---

## 2. Milvus 建模

### 2.1 Collection 命名

```
ws_<hash8(workspace.id)>_default     # db kind workspace default collection
                                     # hash 自 workspace.id (UUID,不可变),
                                     # workspace.name rename 不影响
veda_chunks                          # 现有, fs kind 专用, 不变
veda_summaries                       # 现有, fs kind 专用, 不变
coll_<uuid>                          # 现有, structured collections, 不变
```

新命名空间 `ws_*_default` 与既有完全无重叠。v1 dedicated collection 命名：`ws_<ws>_<dataset>_dim<DIM>_v<VER>`。

### 2.2 Default Collection Schema（每个 db workspace 一份）

```text
字段:
  # 主键 - composite,Milvus PK 强制 upsert dedup
  pk            VARCHAR(128)         PRIMARY KEY
                                     # 格式: "{dataset}:{id}"
                                     # workspace 由 collection 隐含,不入 PK
                                     # API 不暴露 pk,业务方只见 id

  # 用户 record 标识 - 反向冗余,读侧人类可读
  id            VARCHAR(128)         无索引 v0
                                     # 业务方传 → 用;不传 → 服务端生成 UUID

  # 3 层分类 - 全部支持默认值
  dataset       VARCHAR(64)          INVERTED  # 默认 "default"
  category      VARCHAR(64)          INVERTED  # 默认 "default"
  tags          ARRAY<VARCHAR(128)>(8) INVERTED  # 默认 []

  # 行级状态 - search 默认追加 status="active"
  status        VARCHAR(32)          INVERTED  # 默认 "active"

  # 时间戳
  created_at    INT64                INVERTED  # ms,服务端 upsert 时填
  updated_at    INT64                无索引 v0
  expire_at     INT64 nullable       无索引 v0  # 业务方传,v0 不自动清理

  # 向量
  text          VARCHAR(65535, jieba) # 必填,dense + BM25 输入 (UTF-8 bytes)
  vector        FLOAT_VECTOR(<DIM>)   AUTOINDEX, COSINE
                                      # <DIM> = config.embedding.dim
  sparse_vector SPARSE_FLOAT_VECTOR   SPARSE_INVERTED_INDEX, BM25 output

  # 业务自定义
  meta          JSON                  # ≤ 16KB, 默认 {}

Function:
  BM25(input=text, output=sparse_vector)

Collection 配置:
  enableDynamicField = false
```

**索引 v0**（7 个）：vector / sparse_vector / dataset / category / tags / status / created_at
**推后 v1**：id / updated_at / expire_at（中低频，按需补建即可，不要重建 collection）
**API 不暴露**：`pk`（内部 composite）、`status`（pin 到 "active"）、`expire_at`（v0 不实施 TTL）—— 这些是 schema 列但不出现在 request/response

### 2.3 字段约束（app 层校验，per vss §4.3）

- `dataset` / `id`：必须匹配 `[a-zA-Z0-9_-]+`，禁止含 `:`（PK 分隔符）
- `text`：必填，≤ 65535 bytes UTF-8 (Milvus VARCHAR 硬上限，~22k 中文字)
- `meta`：≤ 16KB
- `tags`：≤ 8 个，单个 ≤ 128 字节
- 类型不匹配 → 400 拒写，**不做 coercion**
- PK 不可变；schema 锁死

### 2.4 默认值策略

| 字段 | 默认 | 业务方使用场景 |
|---|---|---|
| `dataset` | `"default"` | 不填 → 写到默认 dataset；workspace 建时 bootstrap 一个 `veda_datasets(name="default")` |
| `id` | 服务端生成 UUID | 不填 → insert 语义（无 dedup，retry 会重复）；要 upsert 必须传 |
| `category` | `"default"` | 中粒度分类，不填即"default"分类 |
| `tags` | `[]` | 多值细标签 |
| `created_at` / `updated_at` | 服务端 upsert 时 `Utc::now()` ms | **PK 重写时同样覆盖**（Pinecone-style：upsert = full replace）。如需"首写时间"语义，业务方自己塞 `meta.first_seen_at` |
| `meta` | `{}` | 业务自由字段 |

### 2.5 PK 不可变契约

- `workspace.id` (UUID) 不可变 → collection 名稳定
- `workspace.name` 可改 → 不影响 collection
- `dataset.name` rename = **数据迁移**（pk 和 dataset 字段烘焙了 name），v0 拒绝
- `id` rename = pk rename = 数据迁移（删旧 + 写新）

### 2.6 创建时序

```
POST /v1/workspaces { kind: "db", name, app_id? } 时:
  1. INSERT veda_workspaces (kind='db', app_id=?)
  2. INSERT veda_datasets (workspace_id=?, name='default', status='active')
     ↑ bootstrap 默认 dataset,业务方可直接用
  3. 推导 collection_name = "ws_" + sha256(workspace.id)[:8] + "_default"
  4. Milvus create_collection(collection_name, schema=§2.2 with DIM=config.embedding.dim)
  5. Milvus create_index × 7 (vector AUTOINDEX, sparse_vector SPARSE_INVERTED_INDEX,
                            dataset/category/tags/status/created_at INVERTED)
  6. Milvus load_collection(collection_name)
  7. 失败回滚: DELETE veda_workspaces 行 + DELETE veda_datasets 行
```

DIM 来自服务端配置（`config/server.toml [embedding]` 块），workspace 创建时业务方不指定也不感知 model/dim。

---

## 3. API 接口（v0 极简版）

### 3.1 控制面

```
POST   /v1/workspaces                 # 已有,扩展支持 kind/app_id (embedding model 从 config 取)
GET    /v1/workspaces                 # 已有
GET    /v1/workspaces/{id}            # 已有
POST   /v1/workspaces/{ws}/datasets   # 新增: 创建 dataset (kind=db only)
GET    /v1/workspaces/{ws}/datasets   # 新增
DELETE /v1/workspaces/{ws}/datasets/{name}  # 新增

POST   /admin/v1/tokens               # 新增 admin 路径,签发 token (返明文一次)
POST   /admin/v1/tokens/{id}/disable  # 新增
```

### 3.2 数据面（仅 db kind workspace 生效）

```
POST   /v1/vectors/upsert
  body: { workspace_id, dataset, records: [{ id, text, meta? }] }
  → 服务端 embed → Milvus upsert → 返 inserted[] + commit_ts

POST   /v1/vectors/search
  body: { workspace_id, dataset, query, top_k, filter? }
  → embed(query) → Milvus search → 返 hits[]

POST   /v1/vectors/query
  body: { workspace_id, dataset, ids }
  → Milvus query by pk → 返 hits[]

POST   /v1/vectors/delete
  body: { workspace_id, dataset, ids }
  → Milvus delete by pk → 返 deleted_count
```

### 3.3 Filter DSL（v0 极简）

```json
{ "filter": { "must": [
  { "field": "meta.category", "op": "eq", "value": "shoes" },
  { "field": "meta.price", "op": "lt", "value": 100 }
]}}
```

支持：`eq / in / gt / gte / lt / lte`，仅 `must`，仅 meta top-level 字段。
完整 9 op + `should / must_not` + 嵌套 + array_contains → v1。

### 3.4 不暴露

- consistency_level / guarantee_timestamp（collection 默认 Bounded，业务方不可调）
- allow_fallback（v0 没 hybrid，谈不上 fallback）
- score_threshold（v1）
- return_fields（v1，目前固定返回 id + pk + score + text + meta）
- cross-dataset / cross-workspace search（v1）
- dedicated collection（v1）

---

## 4. Embedding 流程

```
upsert/search 时 text → EmbeddingService:
  1. cache key = SHA-256(model + ":" + NFC_normalize(trim(text)))
  2. moka cache.try_get_with(key, async {
       upstream OpenAI-compat API (复用 EmbeddingProvider)
     })
  3. 返回 vector

配置:
  cache 实现: moka::future::Cache
  cache 容量: 50,000 (1024 维 f32 ≈ 200MB; 多 model 共享上限)
  cache TTL : write 24h, access 1h
  cache miss 超长 text (>4KB): 不缓存
  cache miss 失败: 不缓存 (try_get_with 自动行为)
```

**单 model**：服务端从 `config/server.toml [embedding]` 加载唯一一份 `EmbeddingProvider`。v0 不支持多 model；未来要多 model 时再加 registry。

---

## 5. v0 范围对照表

| 能力 | v0 | v1 |
|---|---|---|
| workspace.kind 二选一 | ✅ | - |
| token + app_id + allowed_workspaces | ✅ | - |
| dataset 创建/列表/删除 | ✅ | - |
| vectors/{upsert,search,query,delete} | ✅ | - |
| semantic search | ✅ | - |
| embedding gateway + L1 moka | ✅ | - |
| Filter must + eq/in/range | ✅ | - |
| commit_ts 同步返回 | ✅ | - |
| hybrid search (dense + BM25) | - | ✅ |
| fulltext (BM25 only) | - | ✅ |
| cross-dataset / cross-workspace | - | ✅ |
| dedicated collection | - | ✅ |
| Filter 9 op + should/must_not | - | ✅ |
| consistency_level / guarantee_timestamp | - | ✅ |
| score_threshold / return_fields | - | ✅ |
| grants 跨 app 共享 | - | ✅ |
| audit log (DB 表) | - | ✅ |
| idempotency key | - | ✅ |
| per-app / per-token 限流熔断 | - | ✅ |
| L2 Redis embedding cache | - | ✅ |
| async upsert + outbox | - | ✅ |
| bcrypt 切换 | - | 视需要 |
| 进程级隔离 (vector / file 双部署) | - | 视需要 |

---

## 6. Stage 拆解（v0）

> **测试纪律**：每个 Stage DoD 必须包含一份**真实 Milvus / MySQL / embedding 服务的集成测试**通过；纯逻辑单测 OK 用 mock，跨服务边界的禁用 mock（详见 [feedback_testing_use_real_services](../../.claude/projects/-Users-konglingqiao-code-personal-veda/memory/feedback_testing_use_real_services.md)）。

| Stage | 工作量 | 主要文件 | 内容 |
|---|---|---|---|
| 0 | 0.5 天 | `crates/veda-store/src/milvus.rs` 注释 / memory / vss README | 验证 Milvus 2.6+ 部署（done: 2.6.14）；修正版本注释；memory 记录；vss 归档（Joe 手动） |
| 1 | 3 天 | `crates/veda-store/src/mysql.rs`, `crates/veda-server/src/routes/account.rs`, `crates/veda-server/src/auth.rs`, `crates/veda-types/src/errors.rs` | ALTER workspaces (kind/app_id) + ALTER api_keys (app_id/allowed_workspaces/expires_at) + CREATE datasets；workspace 创建 API 加 kind/app_id 参数（model 从 config 取）；auth middleware 加 kind 路径校验；新错误码（WORKSPACE_KIND_MISMATCH / DATASET_NOT_FOUND / WORKSPACE_NOT_FOUND）；admin token 签发 endpoint 占位 |
| 2 | 3 天 | `crates/veda-store/src/milvus.rs`, 新 `crates/veda-server/src/services/workspace_provisioner.rs` | `create_vector_collection(ws_id, dim)`：pk/dataset_id/text(jieba)/vector(AUTOINDEX COSINE)/sparse_vector(BM25)/meta/timestamps + BM25 function + 索引 + load；workspace 创建串 DB+Milvus + 失败回滚；soft delete 路径不动 Milvus |
| 3 | 2 天 | `crates/veda-pipeline/src/embedding.rs` | 加 moka 依赖；在现有 `EmbeddingProvider` 之上包一层 `EmbeddingCache::try_get_with`（key=sha256(model:NFC_norm(trim(text)))，TTL write 24h/access 1h，>4KB skip）；单 provider，不引入 registry |
| 4 | 6 天 | 新 `crates/veda-server/src/routes/{vectors,datasets,admin_tokens}.rs`, 新 `crates/veda-server/src/filter.rs` | dataset CRUD (`POST/GET/DELETE /v1/workspaces/{ws}/datasets`)；admin token 签发/disable；`vectors/{upsert,search,query,delete}` 同步走 Milvus 返 commit_ts；Filter parser (must + eq/in/gt/gte/lt/lte，meta top-level) → Milvus expr；pk 拼装与 charset 校验；limits (batch ≤500, top_k ≤100, text ≤64KiB UTF-8) |
| 5 | 3 天 | 新 `tests/vectors_e2e.rs`, `README.md`, 新 `docs/api/vectors.md`, `ARCHITECTURE.md`, 新 `examples/python_pinecone_demo.py` | E2E：create db ws → dataset → upsert → search → query → delete → search 空；fs API 回归；客户端 demo；ARCHITECTURE 加 fs/structured/vector 三类并列段 |

**总计 ~3-4 周 dedicated work**。calendar 5-6 周。

### 6.1 关于 structured collections 的归属

`veda_collection_schemas`（结构化集合）**归属于 fs workspace**。理由：它原本就是 file 维度的结构化扩展（业务方在文件之外另存结构化记录）。db workspace 不允许建 structured collection——db 的数据模型就是裸向量 + dataset。

三类数据集合的关系：

| | 归属 | 用途 |
|---|---|---|
| files / dentries / chunks / summaries | fs workspace | 文件知识库 |
| structured collections | fs workspace | 文件之外的结构化记录扩展 |
| vector datasets | db workspace | Pinecone-like 裸向量 |

---

## 7. 关键风险与对策

| 风险 | 对策 |
|---|---|
| workspace.kind 创建后不可变 | v0 不支持切换；admin 路径预留"删除重建"流程 |
| collection schema 不可演进，v0 砍 tags/category/status 后 v1 加字段难 | 接受 v1 重建 collection 成本；建迁移工具 |
| embedding API 抖动直接拖慢 upsert | 走 EmbeddingProvider 现有 3 次重试 + 退避；v1 加熔断 |
| 单进程 file + vector 共享：file 路径 panic 影响 vector | v0 接受；**v1 硬门槛：任何外部 app 接入前必须支持 `server_mode = file|vector|all` 或拆双 binary**，独立连接池/限流/熔断（Codex Q4 critical） |
| 单 app 把某 embedding model 打挂连带其他 app | v0 接受；**v1 硬门槛：外部 app 接入前必须有 per-(model, app/token) 并发 + QPS + upstream error budget**（Codex Q9） |
| ws_<hash8> 命名冲突 | hash8 = sha256(workspace_id)[:8]；碰撞概率 1/2^32，创建时 DB check uniq |
| Milvus collection 数量边界 | v0 alpha 期间不到 100 ws；公司接入前 spike Milvus quota（vss design §4.2 要求 ≥1500） |

---

## 8. 已废弃的设计点（追踪）

| 点 | 来源 | 废弃理由 |
|---|---|---|
| 新建 `veda_apps` 表 | v1 草稿 | Joe 决策：token 加 app_id 属性即可，不需要实体表 |
| bcrypt 切换 | v1 草稿 + Codex Q2 | Joe 决策：内部服务不必，继续 SHA-256 |
| Filter DSL 9 op 全套 | v1 草稿 | Codex Q6 + Joe 决策：v0 先跑通 |
| consistency_level 三档 | v1 草稿 | 同上 |
| grants / audit log 表 / idempotency / per-app 限流 | v1 草稿 | 同上 |
| outbox 异步 upsert | v1 草稿 | Codex Q10.3：会破坏 read-your-writes；v0 同步路径 |
| `veda_filterable_fields` 注册 | v1 草稿 | v0 Filter 简化后不需要 |
| Per-dataset embedding_model | v1 草稿 | 简化到 per-workspace；多 model → 多 workspace |
| Per-workspace embedding_model 列 | v2 草稿 | Joe 决策：服务端 config 单一 model 即可，不存表，不引入白名单 |
| HashMap<ModelId, Provider> registry | v2 草稿 | 同上：单 provider 即可，不需要 registry |

---

## 9. 已确认决策

1. **embedding model 单一来源**：服务端 `config/server.toml [embedding]` 已有的 `api_url / model / dim` 字段就是唯一来源。workspace 创建时业务方不指定 model，服务端读取 config 用。**v0 不支持业务方选择 model，不引入白名单概念**——只有一个 model 可用，无需校验。

2. **workspace 删除**：**软删除**。`UPDATE veda_workspaces SET status='archived'`，**不动 Milvus collection**。后续 API 把 status != 'active' 当 404。硬删 + collection drop 推 v1（admin job）。

3. **dataset 删除**：**软删除**。`UPDATE veda_datasets SET status='archived'`，**不动 Milvus 内 dataset_id 对应的数据**。后续 search/upsert/query/delete 进来按 dataset.status 校验，软删的返 404。Milvus 数据由 v1 admin GC job 清。

4. **collection load 策略**：db workspace 创建即 `Milvus.load_collection()`。alpha 期 ws 数量少，内存压力可控；v1 加 lazy load + LRU。

---

## 10. 软删除语义

| 操作 | 影响 |
|---|---|
| DELETE workspace | `veda_workspaces.status='archived'`；Milvus collection 保留；该 ws 下任何 API → 404 |
| DELETE dataset | `veda_datasets.status='archived'`；Milvus 数据保留；该 dataset 任何 API → 404 |
| 同名 recreate | v0 拒（UNIQUE constraint 还在 status=any）；v1 视需求改 unique on (ws, name, status='active') |
| 真删（hard delete + Milvus drop） | v1 admin endpoint |
| 软删后能 list 出来吗 | 不能（默认 filter status='active'）；admin 路径可加 `?include_archived=true` v1 |
