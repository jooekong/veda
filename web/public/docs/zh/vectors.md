# 向量库 API（Vector Workspace）

**向量库**是 Veda 的一种 workspace（`kind=db`），提供 Pinecone 式托管向量检索：写入文本 → 服务端自动嵌入 → 按语义 / 全文检索，支持 meta 过滤。面向业务方应用，走 REST API / SDK 接入。

> 与**文件库**（`kind=fs`，文件存储 + CLI/FUSE）相对，两者数据模型和接入互不相通。本页是向量库的逐字段契约；架构 / 认证 / 控制面全貌见 [详细文档](#/docs/reference)。

---

## 1. 协议与约定

- HTTP + JSON（UTF-8），`Content-Type: application/json`。业务路径在 `/v1/*` 或 `/admin/v1/*` 下。`<BASE>` 由部署决定（示例：`https://veda.ddmc-inc.com`），下文用 `$BASE`。
- **响应信封**：成功 `{ "success": true, "data": {…} }`；失败 `{ "success": false, "error_code": "INVALID_INPUT", "error": "text: must not be empty" }`。`error_code` 是稳定机器码，**客户端只匹配它，别解析 `error` 文案**。部分删除接口返回 `204 No Content` 无体。
- **时间字段两种格式**：控制面对象（Workspace / Dataset 的 `created_at` / `updated_at`）是 **RFC3339 字符串**；向量 hit 的 `created_at` / `updated_at`、`upsert` 的 `commit_ts`、铸 token 的 `expires_at` 是 **int64 毫秒 epoch**。反序列化时别统一。

---

## 2. 认证（务必先读）

凭证走 `Authorization: Bearer <token>`。向量库分两个面，各一种 key：

| 面 | 端点 | 凭证 | 前缀 | 作用域 |
|---|---|---|---|---|
| **数据面** | `/v1/vectors/*` | **workspace key** | `wk_` | 绑定单个 db workspace，分 `read` / `readwrite` |
| **控制面** | `/v1/workspaces*`、`/v1/workspaces/{ws}/datasets*`、`/v1/workspaces/{id}/keys*` | **账号 key** | `vk_` | 账号级（该账号下所有 workspace） |

- **数据面只认 `wk_`**：目标 workspace 由 key 绑定，所以 `/v1/vectors/*` 请求体**不带 `workspace_id`**。只读 `wk_` 可 `search` / `query`，不可 `upsert` / `delete`（→ `403 PERMISSION_DENIED`）。
- **控制面只认 `vk_`**：建 workspace、管 dataset、签 `wk_`。`vk_` 由平台 / 控制台持有，**不下发给业务方**——业务 app 通常只拿一把 `wk_`。
- 把 `vk_` 用到 `/v1/vectors/*`、或把 `wk_` 用到控制面，都按 `401 UNAUTHORIZED` 拒绝。JWT 已彻底移除，不存在第三种凭证。

---

## 3. 接入流程

控制面（建库 + 签 `wk_`）用 `vk_`，由平台 / 控制台完成；业务 app 用拿到的 `wk_` 跑数据面。

```bash
# —— 控制面（平台侧，持账号 vk_）——
ACCOUNT_KEY=vk_...

# 1) 建一个 db workspace（服务端自动 bootstrap default dataset + Milvus collection）
curl -sX POST $BASE/v1/workspaces \
  -H "authorization: Bearer $ACCOUNT_KEY" -H 'content-type: application/json' \
  -d '{"name":"prod-index","kind":"db"}'
# → data: { "id": "<ws_id>", "kind": "db", "status": "active", ... }
WS=<ws_id>

# 2) 为该 workspace 签一把数据面 wk_，交给业务 app
curl -sX POST $BASE/v1/workspaces/$WS/keys \
  -H "authorization: Bearer $ACCOUNT_KEY" -H 'content-type: application/json' \
  -d '{"name":"search-svc","permission":"readwrite"}'
# → data: { "key": "wk_...", "permission": "readwrite" }   # 明文只此一次，请妥存

# —— 数据面（业务 app，只持 wk_；请求体不带 workspace_id）——
WK=wk_...
curl -sX POST $BASE/v1/vectors/upsert \
  -H "authorization: Bearer $WK" -H 'content-type: application/json' \
  -d '{"records":[{"id":"sku-1","text":"Air Jordan 1"}]}'
```

> ⚠️ **别用匿名 onboarding 接 db**：`POST /v1/accounts/anonymous` 一步发账号 + workspace，但它建的是 **`kind=fs`**，向量端点用不了。db 必须显式走第 1 步 `{"kind":"db"}`。

---

## 4. 核心概念

- **Workspace（`kind=db`）**：相当于 Pinecone 的一个 index，一个 workspace 一个独立的 Milvus collection。建库时服务端在单事务里建好 workspace + `default` dataset，再 provision collection（失败回滚）。
- **Dataset**：workspace 内的逻辑分组（如 `products`、`faq`），共享同一 collection，靠标量字段 `dataset` 区分。每个 db workspace 自带 `default` dataset，**不可删除**（省略 `dataset` 时的隐式兜底）。
- **Record**：一行数据。必填 `text`（服务端据此算向量 + 建 BM25 索引），可选 `id` / `category` / `tags` / `meta`。物理主键 `{dataset}:{id}` 由服务端内部组装，**不出现在 wire 上**。

---

## 5. 数据模型（wire 字段）

> 标 `?` 的字段可能不出现。时间格式见 §1。

**Workspace**：`id` / `account_id` / `name` / `status`（`active`\|`archived`）/ `kind`（`fs`\|`db`）/ `app_id`? / `description`? / `created_at` / `updated_at`（RFC3339）

**Dataset**：`id` / `workspace_id` / `name` / `status` / `description`? / `created_at` / `updated_at`（RFC3339）

**WorkspaceKey**（`GET /v1/workspaces/{id}/keys` 列表项）：`id` / `workspace_id` / `account_id` / `name` / `permission`（`read`\|`readwrite`）/ `status`（`active`\|`revoked`）/ `kind` / `created_at`（RFC3339）。明文 `wk_` 仅创建时返回一次。

**VectorSearchHit**（`/v1/vectors/search` 命中项）：

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | string | 记录 id（不含 dataset 前缀），**始终返回** |
| `score` | float | 相关性分数，**越大越相关**，**始终返回**；含义由 `score_type` 决定 |
| `score_type` | string | `cosine`（语义 ANN ~[0,1]）/ `bm25`（全文 ~[0,30+]）/ `rrf`（hybrid 融合 ~[0,0.033]）。**跨 type 不可比** |
| `dataset` | string | 所属 dataset |
| `category` | string | 分类 |
| `tags` | string[] | 标签 |
| `text` | string | 原文 |
| `meta` | object | 自定义 JSON |
| `created_at` / `updated_at` | int64 | 毫秒 epoch |

> `dataset` 及之后的字段属于**可投影字段**：不传 `output_fields` 时全部返回；传了 `output_fields` 时只返回所列的（`id` / `score` / `score_type` 永远返回，不用列）。

**VectorRecordHit**（`/v1/vectors/query` 命中项）：同上但**没有 `score` / `score_type`**（按 id 直查，非排序匹配）。

---

## 6. 端点参考（🟦 `wk_`）

四个端点目标 workspace 由 `wk_` 绑定，公共参数仅 `dataset`?（省略取 `default`）。`upsert` / `delete` 需 `readwrite`。

### POST `/v1/vectors/upsert`
按 `(dataset, id)` 插入或整行替换。单次最多 **500** 条。

```json
{
  "dataset": "products",
  "write_mode": "upsert",
  "records": [
    { "id": "sku-1", "text": "Air Jordan 1",
      "category": "shoes", "tags": ["sale","new"], "meta": {"price": 1299} }
  ]
}
```

- 每条 record 除 `text` 外都有默认：`id`→服务端 UUID（**走 insert，非幂等**，见 §9）、`category`→`"default"`、`tags`→`[]`、`meta`→`{}`。
- 顶层 `write_mode`?：`"upsert"`（默认，幂等安全）/ `"insert"`（跳过查重、~3x 速、**调用方保证 pk 唯一**；重复 pk 是 Milvus 未定义行为，见 §9）。

响应 200：`{ "ids": ["sku-1"], "commit_ts": 1735689600000 }`
- `ids`：实际写入的 id，按请求顺序、**已对同批重复 id 去重**（last-wins），可能比 `records` 短。省略 `id` 的记录在这里**首次也是唯一一次**暴露服务端生成的 UUID，务必留存。
- `commit_ts`：服务端写完时刻（毫秒 epoch），用于同机 read-your-writes（非分布式可比逻辑时钟）。
- 错误：`400 INVALID_INPUT`（字段校验，含单字段长度 / 字符集超限、空 `records`）、`403 PERMISSION_DENIED`（只读 `wk_`）、`404 NOT_FOUND`（dataset 不存在 / 已归档）、`413 PAYLOAD_TOO_LARGE`（**仅** `records` 条数 >500）、`500 EMBEDDING_FAILED`。

### POST `/v1/vectors/search`
检索目标 dataset（**隐式锁定**，v0 不支持跨 dataset）。

```json
{
  "dataset": "products",
  "query": "sneakers under 1500",
  "mode": "semantic",
  "top_k": 10,
  "min_score": 0.4,
  "output_fields": ["text", "meta"],
  "filter": { "must": [
    { "field": "meta.price", "op": "lt", "value": 1500 },
    { "field": "meta.category", "op": "eq", "value": "shoes" }
  ] }
}
```

| `mode` | 行为 | 嵌入 query？ | `score_type` | 分数范围 |
|---|---|:--:|---|---|
| `hybrid`（**默认**） | 稠密 ANN + BM25，RRF 融合 | 是 | `rrf` | ~[0, 0.033] |
| `semantic` | 对嵌入后的 query 做稠密 ANN | 是 | `cosine` | ~[0, 1] |
| `fulltext` | 对分词后的 `text` 做 BM25 | 否 | `bm25` | ~[0, 30+] |

- `mode`? 默认 `hybrid`；`top_k`? 默认 10、最大 100；`filter`? 见 §7。
- `output_fields`?：投影白名单，子集 ∈ `{dataset, category, tags, text, meta, created_at, updated_at}`。省略=全返；`id` / `score` / `score_type` 永远返回，**不要列入**（列了非法字段 → `400`）。
- `min_score`?：**相关度下限**，丢掉低于它的命中。**仅 `semantic`(cosine) / `fulltext`(bm25) 生效**；与 `hybrid`（含默认 mode）同用即 `400`——RRF 是排名不是相关度，要门槛请用 `top_k` 或显式 `mode=semantic`。在 `top_k` 之后裁剪，故结果可能 **少于 `top_k`**。需按模型校准（dense 下无关文本也常 ~0.15–0.25，有效阈值要明显更高，如 0.4–0.6，没有通用的 "0.5"）。
- `hybrid` 失败直接报错，**不静默降级到 semantic**。`fulltext` 跳过嵌入调用（更便宜，也是唯一不依赖 embedding model 的 mode）。

响应 200：`{ "hits": VectorSearchHit[] }`。
- 错误：`400 INVALID_INPUT`（query 空或 >65535 字节 / `top_k=0` / `filter` 非法 / `output_fields` 含非法字段 / `min_score` 非有限值或与 `hybrid` 同用）、`404 NOT_FOUND`（dataset）、`413 PAYLOAD_TOO_LARGE`（`top_k>100`）、`500 EMBEDDING_FAILED`（semantic/hybrid 嵌入失败）。

### POST `/v1/vectors/query`
按 id 直查。**不保证顺序；不存在的 id 静默跳过（不报错）**。单次最多 **500** 个 id。

```json
{ "dataset": "products", "ids": ["sku-1","sku-2"], "output_fields": ["text"] }
```

响应 200：`{ "hits": VectorRecordHit[] }`（无 `score`）。`output_fields`? 同 search。
- 错误：`400 INVALID_INPUT`（`ids` 空 / id 非法）、`404 NOT_FOUND`（dataset）、`413 PAYLOAD_TOO_LARGE`（>500）。

### POST `/v1/vectors/delete`
按 id 硬删。单次最多 **500** 个 id。

```json
{ "dataset": "products", "ids": ["sku-1","sku-2"] }
```

响应 200：`{ "delete_count": 2 }`。
> ⚠️ `delete_count` 是 Milvus 创建的 **tombstone 数 = `len(ids)`**，与「实际存在并被删的行数」**无关**。要区分请先 `query`。
- 错误：`400 INVALID_INPUT`（`ids` 空 / id 非法）、`403 PERMISSION_DENIED`（只读 `wk_`）、`404 NOT_FOUND`（dataset）、`413 PAYLOAD_TOO_LARGE`（>500）。

---

## 7. Filter DSL（v0）

Qdrant 风格的严格子集，仅用于 `/v1/vectors/search` 的 `filter`：

- 只有 `must`（无 `should` / `must_not`），所有 clause **AND** 组合，并与基础 scope（`dataset == "X" && status == "active"`）合并。
- `field` 只能是 `meta.<顶层key>`，key 须 `[a-zA-Z0-9_-]+`，**不支持嵌套**（`meta.a.b` → 400）。平台字段（`dataset` / `tags` / `status` 等）**不可经此 DSL 过滤**。
- `op`：`eq` \| `in` \| `gt` \| `gte` \| `lt` \| `lte`。
- `value`：`eq` 接受标量 string / number / bool；范围 `gt`/`gte`/`lt`/`lte` **仅** number / string（bool/null/数组/对象 → 400）；`in` 为标量数组，**非空且 ≤100 项**。

```json
{ "must": [
  { "field": "meta.brand", "op": "in",  "value": ["nike", "adidas"] },
  { "field": "meta.price", "op": "gte", "value": 500 }
] }
```

---

## 8. 限制与校验

| 对象 | 限制 |
|---|---|
| `dataset` / `id` 字符集 | `[a-zA-Z0-9_-]+`，禁止 `:`（物理 PK 分隔符） |
| `dataset` | 非空，≤ 64 字节 |
| `id` | 非空，≤ 64 字节（且 `{dataset}:{id}` 合成后 ≤ 128 字节） |
| `text` | 非空，≤ 65535 字节（UTF-8，Milvus VARCHAR 硬上限）；更大需客户端分片 |
| `meta` | JSON 序列化后 ≤ 16 KB；建议用 object（filter 仅能按 `meta.<key>` 过滤） |
| `tags` | ≤ 8 个，单个非空、≤ 128 字节 |
| `category` | 非空，≤ 64 字节 |
| `upsert.records` | 非空，≤ 500 / 次 |
| `query.ids` / `delete.ids` | 非空，≤ 500 / 次 |
| `search.top_k` | 默认 10，最大 100 |
| `filter` `in` 数组 | 非空，≤ 100 |
| 列表 `limit` | 默认 100，最大 200（超出静默截断） |

所有字段校验在任何 Milvus 写入**之前**完成。**单字段超限（长度 / 字符集 / 非空）→ `400 INVALID_INPUT`**（`error` 形如 `<field>: <reason>`）；**仅批量条数超限（`records` / `ids` >500、`top_k` >100）→ `413 PAYLOAD_TOO_LARGE`**。

---

## 9. 幂等性与重试

写入语义由 `write_mode`（默认 `upsert`）+ 是否自带 `id` 共同决定：

| `write_mode` | `id` | 行为 | 幂等 |
|---|---|---|:--:|
| `upsert`（默认） | 自带 | 按 `(workspace, dataset, id)` 原地整行替换；时间戳重置，`meta`/`tags`/`text` 全量覆盖 | ✅ |
| `upsert`（默认） | 省略 | 服务端 UUID → 走 insert 快路径（UUID 不可能撞） | ❌ |
| `insert` | 自带 | 直接 insert、**跳过查重**、~3x 速；**调用方保证 `id` 唯一** | ❌ |
| `insert` | 省略 | 服务端 UUID → insert | ❌ |

> **`insert` 拿安全换速度**：Milvus insert 不检查 pk 唯一性，重复 pk 是 **未定义行为**——物理累积多行且 compaction **不自动清**，`query`/`search` 返回哪条 unknown。仅用于 id 天然唯一的场景（自增 / UUID / 一次性导入）。**怕重试或会重导的管道必须用默认 `upsert`**。

- **同批重复 `id`**：服务端去重、**last-wins**（取最后一次值，保留首次位置），无报错，响应 `ids` 反映去重后结果。
- **混合批不原子**：一个默认 upsert 请求里混了「带 id」和「无 id」记录时，会拆成两次 Milvus 调用（无原子性）。要安全重试，请整批都自带 `id`。
- **commit_ts**：服务端本地时间近似，仅用于同机 read-your-writes。

---

## 10. 错误码

失败固定 `{ "success": false, "error_code": "...", "error": "..." }`。**只匹配 `error_code`**。

| `error_code` | HTTP | 含义 |
|---|---:|---|
| `INVALID_INPUT` | 400 | 校验失败（字符集 / 长度 / 缺字段 / 单字段超限 / 非法 `output_fields` / `min_score` 误用）。`error` 带 `<field>: <reason>` |
| `WORKSPACE_KIND_MISMATCH` | 400 | 向量 API 打到了 fs workspace（或反之） |
| `CANNOT_DELETE_DEFAULT_DATASET` | 400 | 拒绝删除 `default` dataset |
| `UNAUTHORIZED` | 401 | 缺失 / 无效 / 过期 / 用错面的 token |
| `PERMISSION_DENIED` | 403 | 只读 `wk_` 调 `upsert`/`delete`；或 `vk_` 操作他账号资源 |
| `NOT_FOUND` | 404 | workspace / dataset / key / token 不存在或已归档 |
| `ALREADY_EXISTS` | 409 | workspace / dataset 同名（大小写不敏感）/ 邮箱已注册 |
| `PAYLOAD_TOO_LARGE` | 413 | **仅**批量条数超限（`records`/`ids` >500、`top_k` >100） |
| `QUOTA_EXCEEDED` | 429 | 向量 API 当前不返回，保留 |
| `EMBEDDING_FAILED` | 500 | 服务端嵌入上游错误（`error` 文案被抹成 `internal server error`，只 `error_code` 稳定） |
| `INTERNAL` | 500 | 存储 / 死锁 / 未预期错误的兜底，故意不透出细节 |

---

## 11. 控制面端点（🔑 `vk_`）

数据面凭证 `wk_` 与 dataset 的生命周期都在控制面，用账号 `vk_`：

| 方法 | 路径 | 用途 |
|---|---|---|
| POST | `/v1/accounts` | 注册账号 → `{ account_id, api_key }` |
| POST | `/v1/accounts/login` | 登录换新 `vk_` |
| POST | `/v1/workspaces` | 建 workspace（`{"kind":"db"}`） |
| GET | `/v1/workspaces` | 列 workspace（分页） |
| DELETE | `/v1/workspaces/{id}` | 软删 workspace（200） |
| POST | `/v1/workspaces/{id}/keys` | 签发数据面 `wk_`（`read`/`readwrite`，明文仅一次） |
| GET | `/v1/workspaces/{id}/keys` | 列 `wk_` 元数据（不分页） |
| DELETE | `/v1/workspaces/{id}/keys/{key_id}` | 撤销 `wk_`（204） |
| POST | `/v1/workspaces/{ws}/datasets` | 建 dataset（201） |
| GET | `/v1/workspaces/{ws}/datasets` | 列 dataset（分页） |
| DELETE | `/v1/workspaces/{ws}/datasets/{name}` | 软删 dataset（204，不能删 `default`） |
| POST | `/admin/v1/tokens` | 铸 scoped `vk_` 服务令牌（201，`token` 仅一次） |
| POST | `/admin/v1/tokens/{id}/disable` | 撤销令牌（204） |

完整请求 / 响应字段、apps 平台面、运维端点见 [详细文档](#/docs/reference)。

---

## 12. 上线前注意

- **嵌入吞吐受云商 QPM 硬限**，无客户端并发闸：大批量写 / 高并发检索请控制并发、分批退避，或对纯关键词用 `mode=fulltext`（不走嵌入）。
- **写入吞吐 << 读取**：批量写优先 `write_mode=insert`（id 唯一时）+ ≤500/次。
- 软删 workspace / dataset **不回收 Milvus 向量**。
- Java SDK 仍是旧 `vk_` 契约，**未适配 `wk_`**；现阶段建议直接按本页裸 HTTP 接入。
