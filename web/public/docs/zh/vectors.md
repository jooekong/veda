# 向量库 API（Vector Workspace）

**向量库**是 Veda 的一种 workspace 类型（`kind=db`），提供 Pinecone 式的托管向量检索：写入文本 → 服务端自动嵌入 → 按语义检索，支持 meta 过滤。面向业务方应用，通过 REST API / SDK 接入。

> 与**文件库**（File Workspace，`kind=fs`，文件存储 + CLI/FUSE）相对。两者数据模型和接入方式不同、互不相通——文件库的 `wk_` / JWT / FUSE 不适用于向量库。

---

## 1. 协议与约定

- HTTP + JSON（UTF-8），`Content-Type: application/json`。
- 所有业务端点在 `/v1/*` 或 `/admin/v1/*` 下。`<BASE_URL>` 由部署决定，下文示例用 `$BASE` 代指。

**响应信封**：成功
```json
{ "success": true, "data": { /* 随接口而定 */ } }
```
失败
```json
{ "success": false, "error_code": "INVALID_INPUT", "error": "text: must not be empty" }
```
- `error_code` 是**稳定的机器可读码**，只在失败时出现，**客户端只匹配 `error_code`，不要解析 `error` 文案**。
- 部分删除接口返回 `204 No Content` 无响应体（见下）。

**时间字段两种格式，反序列化时注意区分**：

| 出现位置 | 类型 | 示例 |
|---|---|---|
| 控制面对象（Workspace / Dataset 的 `created_at` / `updated_at`） | RFC3339 字符串 | `"2026-05-29T12:34:56Z"` |
| 向量 hit 的 `created_at` / `updated_at`、`upsert` 的 `commit_ts`、铸 token 请求的 `expires_at` | int64 毫秒 epoch | `1735689600000` |

---

## 2. 认证

凭证通过 `Authorization: Bearer <token>` 携带。**向量库只认账号级 `vk_`**：

| 凭证 | 前缀 | 用于向量库？ |
|---|---|:--:|
| 账号 key / 服务令牌 | `vk_` | ✅ 是 |
| workspace key | `wk_` | ❌ 否（文件库专用） |
| workspace JWT | 无前缀 | ❌ 否（文件库专用） |

把 `wk_` 或 JWT 用在向量库端点上会返回 `401 UNAUTHORIZED`。

**作用域**：服务令牌（`POST /admin/v1/tokens`）可带 `allowed_workspaces` 限定可操作的 workspace；向量端点若**省略** `workspace_id`，仅当 token 的 `allowed_workspaces` 恰好 1 个时才隐式取它，否则必须显式传。

---

## 3. 快速接入

全程只用 `vk_`：

```bash
# 1) 注册账号，拿账号级 vk_（或用已有账号 /v1/accounts/login）
curl -sX POST $BASE/v1/accounts -H 'content-type: application/json' \
  -d '{"name":"acme","email":"ops@acme.com","password":"<pw>"}'
# → data: { "account_id": "...", "api_key": "vk_..." }

ACCOUNT_KEY=vk_...

# 2) 建一个向量库（服务端自动 bootstrap 一个 default dataset + Milvus collection）
curl -sX POST $BASE/v1/workspaces \
  -H "authorization: Bearer $ACCOUNT_KEY" -H 'content-type: application/json' \
  -d '{"name":"prod-index","kind":"db"}'
# → data: { "id": "<ws_id>", "kind": "db", "status": "active", ... }

WS=<ws_id>

# 3)（可选）为业务 app 铸一个限定到该向量库的服务令牌
curl -sX POST $BASE/admin/v1/tokens \
  -H "authorization: Bearer $ACCOUNT_KEY" -H 'content-type: application/json' \
  -d "{\"app_id\":\"search-svc\",\"name\":\"prod\",\"allowed_workspaces\":[\"$WS\"]}"
# → data: { "id": "...", "token": "vk_..." }   # token 仅此一次可见

# 4) 写入 / 检索
curl -sX POST $BASE/v1/vectors/upsert \
  -H "authorization: Bearer $ACCOUNT_KEY" -H 'content-type: application/json' \
  -d "{\"workspace_id\":\"$WS\",\"records\":[{\"id\":\"sku-1\",\"text\":\"Air Jordan 1\"}]}"
```

> ⚠️ 匿名 onboarding（`POST /v1/accounts/anonymous`）建的是**文件库**（`kind=fs`），向量库必须显式走第 2 步 `{"kind":"db"}`。

---

## 4. 核心概念

- **向量库（Workspace, `kind=db`）**：相当于 Pinecone 的一个 index，一库一个独立的 Milvus collection。建库时服务端在单事务里建好 workspace + `default` dataset，再 provision collection（失败回滚）。
- **Dataset**：库内逻辑分组（如 `products`、`faq`），共享同一 collection，靠标量 `dataset` 字段区分。每个向量库自带 `default` dataset，**不可删除**（省略 `dataset` 时的隐式兜底）。
- **Record**：一行数据。必填 `text`（同时建 BM25 索引），可选 `id` / `category` / `tags` / `meta`，向量由服务端对 `text` 计算。物理主键 `{dataset}:{id}` 由服务端内部组装，不出现在 wire 上。

---

## 5. 数据模型（wire 字段）

**Workspace**：`id` / `account_id` / `name` / `status`（`active`\|`archived`）/ `kind`（`fs`\|`db`）/ `app_id`? / `created_at` / `updated_at`（RFC3339）

**Dataset**：`id` / `workspace_id` / `name` / `status` / `created_at` / `updated_at`（RFC3339）

**VectorSearchHit**（`/v1/vectors/search` 命中）：

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | string | 记录 id（不含 dataset 前缀） |
| `dataset` | string | 所属 dataset |
| `category` | string | 分类 |
| `tags` | string[] | 标签 |
| `text` | string | 原文 |
| `meta` | object | 自定义 JSON |
| `created_at` / `updated_at` | int64 | 毫秒 epoch |
| `score` | float | COSINE 相似度，**越大越相似** |

**VectorRecordHit**（`/v1/vectors/query` 命中）：同上但**无 `score`**。

---

## 6. 数据面端点

公共参数：`workspace_id`?（省略规则见 §2）、`dataset`?（省略取 `default`）。目标必须是向量库（`kind=db`）。

### POST `/v1/vectors/upsert`
按 `(dataset, id)` 插入或整行替换。单次最多 **500** 条。
```json
{
  "workspace_id": "<ws_id>",
  "dataset": "products",
  "records": [
    { "id": "sku-1", "text": "Air Jordan 1",
      "category": "shoes", "tags": ["sale","new"], "meta": {"price": 1299} }
  ]
}
```
每条除 `text` 外都有默认：`id`→服务端 UUID（**insert-only，非幂等**，见 §9）、`category`→`"default"`、`tags`→`[]`、`meta`→`{}`。

响应：`{ "ids": ["sku-1"], "commit_ts": 1735689600000 }`
- `ids`：实际写入的 id，按请求顺序、**已对同批重复 id 去重**（last-wins），可能比 `records` 短。省略 `id` 的记录在这里**首次也是唯一一次**暴露服务端生成的 UUID，务必留存。
- `commit_ts`：服务端写完时刻（毫秒 epoch），用于同机 read-your-writes。
- 错误：`400 INVALID_INPUT`（字段校验，含单字段长度/字符集超限）、`413 PAYLOAD_TOO_LARGE`（**仅** records 条数 >500）、`500 EMBEDDING_FAILED`。

### POST `/v1/vectors/search`
对 `query`（服务端嵌入）做稠密 ANN，**隐式锁定目标 dataset**（v0 不支持跨 dataset）。
```json
{
  "workspace_id": "<ws_id>", "dataset": "products",
  "query": "sneakers under 1500", "top_k": 10,
  "filter": { "must": [
    { "field": "meta.price", "op": "lt", "value": 1500 },
    { "field": "meta.category", "op": "eq", "value": "shoes" }
  ] }
}
```
`top_k` 默认 10、最大 100；`filter` 见 §7。响应：`{ "hits": VectorSearchHit[] }`。
- 错误：`400 INVALID_INPUT`（query 为空或 >65535 字节 / top_k=0 / filter 非法）、`413 PAYLOAD_TOO_LARGE`（top_k>100）、`500 EMBEDDING_FAILED`。

### POST `/v1/vectors/query`
按 id 直查。**不保证顺序；不存在的 id 静默跳过**。单次最多 **500** 个 id。
```json
{ "workspace_id": "<ws_id>", "dataset": "products", "ids": ["sku-1","sku-2"] }
```
响应：`{ "hits": VectorRecordHit[] }`。

### POST `/v1/vectors/delete`
按 id 硬删。单次最多 **500** 个 id。
```json
{ "workspace_id": "<ws_id>", "dataset": "products", "ids": ["sku-1","sku-2"] }
```
响应：`{ "delete_count": 2 }`。
> ⚠️ `delete_count` 是 Milvus 创建的 tombstone 数 = `len(ids)`，与「实际存在并被删的行数」无关。要区分请先 `query`。

---

## 7. 控制面端点

| 方法 | 路径 | 用途 |
|---|---|---|
| POST | `/v1/accounts` | 注册账号 → `{account_id, api_key}` |
| POST | `/v1/accounts/login` | 登录换新 `vk_` |
| POST | `/v1/workspaces` | 建 workspace（`{"kind":"db"}` 为向量库），返回 Workspace |
| GET | `/v1/workspaces` | 列 workspace（分页，见 §11） |
| DELETE | `/v1/workspaces/{id}` | 软删 workspace（200 + `{success:true}`） |
| POST | `/v1/workspaces/{ws}/datasets` | 建 dataset（201，返回 Dataset） |
| GET | `/v1/workspaces/{ws}/datasets` | 列 dataset（分页） |
| DELETE | `/v1/workspaces/{ws}/datasets/{name}` | 软删 dataset（204）；不能删 `default`（含大小写，返回 400） |
| POST | `/admin/v1/tokens` | 铸 `vk_` 服务令牌（201，`token` 仅此一次返回） |
| POST | `/admin/v1/tokens/{id}/disable` | 撤销令牌（204） |

**铸服务令牌** `POST /admin/v1/tokens`：
```json
{ "app_id": "search-svc", "name": "prod",
  "allowed_workspaces": ["<ws_id>"], "expires_at": 1767225600000 }
```
`allowed_workspaces`? 省略=账号内不限；`expires_at`? 为毫秒 epoch，省略=永不过期。响应 `{ "id": "...", "token": "vk_..." }`。

---

## 8. Filter DSL（v0）

Qdrant 风格的严格子集，仅用于 `/v1/vectors/search` 的 `filter`：

- 只有 `must`（无 `should` / `must_not`），所有 clause **AND** 组合，并与基础 scope（`dataset == "X" && status == "active"`）合并。
- `field` 只能是 `meta.<key>`，key 须 `[a-zA-Z0-9_-]+`，**不支持嵌套**（`meta.a.b` 报错）。平台字段（`dataset`/`tags`/`status` 等）不可经此 DSL 过滤。
- `op`：`eq` \| `in` \| `gt` \| `gte` \| `lt` \| `lte`。
- `value`：
  - `eq`：标量 string / number / **bool**；
  - 范围 `gt`/`gte`/`lt`/`lte`：仅 **number / string**（bool / null / 数组 / 对象会 400）；
  - `in`：标量数组，**非空且 ≤100 项**（解析期展开为 OR 链）。

```json
{ "must": [
  { "field": "meta.brand", "op": "in",  "value": ["nike", "adidas"] },
  { "field": "meta.price", "op": "gte", "value": 500 }
] }
```

---

## 9. 限制与校验

| 对象 | 限制 |
|---|---|
| `dataset` / `id` | `[a-zA-Z0-9_-]+`，禁止 `:`；各 ≤ 64 字节（合成 `{dataset}:{id}` ≤ 128） |
| `text` | 非空，≤ 65535 字节（UTF-8）；更大需客户端分片 |
| `meta` | 任意 JSON value（推荐 object——filter 只能按 `meta.<key>` 过滤），序列化后 ≤ 16 KB |
| `tags` | ≤ 8 个，单个 ≤ 128 字节，不可空串 |
| `category` | 非空，≤ 64 字节 |
| `upsert.records` / `query.ids` / `delete.ids` | 非空，≤ 500 / 次 |
| `search.top_k` | 默认 10，最大 100 |
| `filter` `in` 数组 | 非空，≤ 100 |
| 列表 `limit` | 默认 100，最大 200（超出静默截断） |

**单字段超限（长度 / 字符集 / 非空）返回 `400 INVALID_INPUT`**；**仅批量条数超限（records / ids >500、top_k >100）返回 `413 PAYLOAD_TOO_LARGE`**。

---

## 10. 幂等性与重试

**upsert 是否幂等取决于是否自带 `id`**：

| 模式 | `id` | 重试同一请求 |
|---|---|---|
| 自带 id | 调用方提供 | **幂等**：同 `(workspace, dataset, id)` 原地整行替换；`created_at`/`updated_at` 每次重置，字段全量覆盖为最新 |
| 省略 id | 服务端生成 UUID | **非幂等**：每次重试新建一条不同 UUID 的记录；网络重试会重复写入 |

> 怕重试的调用方**必须自带 `id`**（内容哈希或 UUIDv7 客户端生成）。

**同一次 upsert 内重复 `id`**：服务端去重，**last-wins**（取最后一次出现的值，保留首次位置），无报错，响应 `ids` 反映去重后结果。

---

## 11. 分页

`GET /v1/workspaces` 与 `GET /v1/workspaces/{ws}/datasets` 用游标分页：
- `limit`：每页条数，默认 100，最大 200（超出静默截断）。
- `after`：上一页 `next_cursor`，不透明字符串。

```json
{ "success": true, "data": {
  "items": [ /* ... */ ], "has_more": true, "next_cursor": "<下次 after>"
} }
```
`has_more=false` 时 `next_cursor` 不出现。排序稳定但实现定义（按行 id 升序），非业务有意义排序。

---

## 12. 错误码

失败响应固定 `{ "success": false, "error_code": "...", "error": "..." }`。**只匹配 `error_code`**。

| `error_code` | HTTP | 含义 |
|---|---:|---|
| `INVALID_INPUT` | 400 | 通用校验失败（字符集 / 长度 / 缺字段 / 单字段超限），`error` 携带 `<field>: <reason>` |
| `WORKSPACE_KIND_MISMATCH` | 400 | 向量端点打到了文件库（或反之） |
| `CANNOT_DELETE_DEFAULT_DATASET` | 400 | 拒绝删除 `default` dataset（含大小写） |
| `UNAUTHORIZED` | 401 | 缺失 / 无效 / 过期的 bearer token |
| `PERMISSION_DENIED` | 403 | 已认证，但 `allowed_workspaces` 不覆盖目标，或操作了他账号资源 |
| `NOT_FOUND` | 404 | workspace / dataset / token 不存在或已归档 |
| `ALREADY_EXISTS` | 409 | dataset 同名冲突（大小写不敏感）/ 邮箱已注册 |
| `PAYLOAD_TOO_LARGE` | 413 | 仅批量条数超限（records / ids >500、top_k >100） |
| `EMBEDDING_FAILED` | 500 | 服务端嵌入上游错误 |
| `INTERNAL` | 500 | 存储 / 未预期错误的兜底 |
