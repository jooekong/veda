# Veda 向量服务 API（db workspace）

> **对外权威契约**见 `web/public/docs/zh/reference.md` + `web/public/docs/zh/vectors.md`（随 console 发布）；本文件是 repo 内供 agent / SDK 开发的参考，两处有出入时以 web 版为准。

面向业务方接入的完整 HTTP API 参考。目标读者：写 SDK / 集成的工程师，以及阅读本文生成代码的 coding agent。

- 协议：HTTP/1.1，JSON over UTF-8，`Content-Type: application/json`。
- 风格：Pinecone 式向量数据面 + 轻量控制面。v0 契约，设计锁定于
  [`docs/vectors-merge-plan.md`](../vectors-merge-plan.md)；向量数据面的语义细节（幂等、filter DSL）另见 [`docs/api/vectors.md`](./vectors.md)，本文为完整接口参考、与之同源。
- 本文**只覆盖 db workspace（向量服务）**。fs workspace 的文件 / 全文检索 / SQL / 摘要接口（`/v1/fs/*`、`/v1/search`、`/v1/sql`、`/v1/abstract`、`/v1/overview`）见 [§9 不属于本 API 的端点](#9-不属于本-api-的端点)。
- **Java SDK**：数据面 4 端点（upsert/search/query/delete）的官方 Java 封装见 [`sdk/java`](../../sdk/java/README.md)（Java 8 + Jackson + OkHttp）。Python 示例见 [`examples/python_pinecone_demo.py`](../../examples/python_pinecone_demo.py)，Java 示例见 [`examples/java`](../../examples/java)。

---

## 目录

1. [全局约定](#1-全局约定)
2. [认证模型（务必先读）](#2-认证模型务必先读)
3. [接入流程](#3-接入流程)
4. [核心概念](#4-核心概念)
5. [数据模型（wire 字段）](#5-数据模型wire-字段)
6. [端点参考](#6-端点参考)
7. [Filter DSL（v0）](#7-filter-dslv0)
8. [限制与校验](#8-限制与校验)
9. [不属于本 API 的端点](#9-不属于本-api-的端点)
10. [错误码](#10-错误码)
11. [幂等性与重试](#11-幂等性与重试)
12. [分页](#12-分页)
13. [附录：运维端点](#13-附录运维端点)

---

## 1. 全局约定

### Base URL

`<BASE_URL>` 由服务端 `listen` 配置决定（例：`http://10.79.51.161:<port>`）。下文示例统一用环境变量 `$BASE` 代指。所有业务路径在 `/v1/*` 或 `/admin/v1/*` 下。

### 响应信封

**所有有响应体的接口**统一用同一信封 `ApiResponse<T>`：

成功：
```json
{ "success": true, "data": { /* T，随接口而定 */ } }
```

失败：
```json
{ "success": false, "error_code": "INVALID_INPUT", "error": "text: must not be empty" }
```

- `error_code`：**稳定的机器可读码**，永远只在失败时出现。**客户端只应匹配 `error_code`，不要解析 `error` 文案**（文案随时可能改）。
- `error`：人类可读描述，仅供日志 / 调试。
- 成功响应没有 `error_code` / `error`；失败响应没有 `data`。
- 例外：部分删除接口返回 **204 No Content 无响应体**（见 §6），不带信封；运维端点（§13 的 `/healthz`、`/v1/ready`、`/v1/metrics`）也各有自己的格式，不走此信封。

### HTTP 状态码

成功路径用 `200`（一般）、`201`（create dataset / create token）、`204`（delete dataset / disable token）。失败时 HTTP 状态与 `error_code` 一一对应，见 [§10](#10-错误码)。

### 时间字段格式 ⚠️

两种格式并存，反序列化时务必区分：

| 出现位置 | 类型 | 示例 |
|---|---|---|
| 控制面对象（Workspace / Dataset 的 `created_at` / `updated_at`） | **RFC3339 字符串** | `"2026-05-29T12:34:56Z"` |
| 向量 hit 的 `created_at` / `updated_at`、`upsert` 的 `commit_ts`、`POST /admin/v1/tokens` 请求的 `expires_at` | **int64 毫秒 epoch** | `1735689600000` |

SDK 生成类型时不要把这两类统一成一个时间类型。

---

## 2. 认证模型（务必先读）

凭证通过 `Authorization: Bearer <token>` 头携带。db workspace 分**两个面**，各用一种 key：

| 面 | 端点 | 凭证 | 前缀 | 作用域 |
|---|---|---|---|---|
| **数据面** | `/v1/vectors/*` | **Workspace key** | `wk_` | 绑定单个 db workspace，分 `read` / `readwrite` |
| **控制面** | `/v1/workspaces*`、`/v1/workspaces/{ws}/datasets*`、`/v1/workspaces/{id}/keys*` | **Account key** | `vk_` | 账号级（该账号下所有 workspace） |

> **数据面只认 `wk_`**（内部 `AuthDbWorkspace`）：目标 workspace 由 key 绑定，所以 `/v1/vectors/*` 请求体**不带 `workspace_id`**。read-only `wk_` 可 `search` / `query`，不可 `upsert` / `delete`（→ `403 PERMISSION_DENIED`）。
> **控制面只认 `vk_`**（内部 `AuthAccount`）：建 workspace、管 dataset、签发 `wk_`。`vk_` 由平台 / 控制台持有，**不下发给数据面业务方**。
>
> 把 `vk_` 用到 `/v1/vectors/*`、或把 `wk_` 用到控制面，都会按无效凭证 `401 UNAUTHORIZED` 拒绝。

### 业务 app 的标准持有物

业务 app 通常**只拿到一把 `wk_`**（平台为某个 db workspace 签发）。建 workspace / dataset、签 `wk_` 都是平台侧用 `vk_` 完成的控制面动作，业务 app 不接触 `vk_`。

> **平台接入**：公司 AI Platform 走另一套按 `app_id` 的控制面（`/v1/apps/{app_id}/...`，鉴权外移到平台网关），详见平台管理 API 文档。本文描述的 `vk_` 控制面是当前直连形态。

---

## 3. 接入流程

控制面（建 workspace + 签 `wk_`）用 `vk_`，由平台 / 控制台完成；业务 app 用拿到的 `wk_` 跑数据面。

```bash
# —— 控制面（平台侧，持账号 vk_）——
ACCOUNT_KEY=vk_...   # 平台持有的账号 key（注册见 §6.1）

# 1) 建一个 db workspace（服务端自动 bootstrap 一个 default dataset + Milvus collection）
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

> ⚠️ **不要用匿名 onboarding 接 db**：`POST /v1/accounts/anonymous` 一步发账号 + workspace，但它建的是 **`kind=fs`** workspace，向量端点用不了。db 必须显式走上面第 1 步建 `kind=db`。

---

## 4. 核心概念

- **Workspace（`kind=db`）**：相当于 Pinecone 的一个 index，一个 workspace 一个独立的 Milvus collection。`POST /v1/workspaces {kind:"db"}` 时服务端在单事务里建好 workspace + `default` dataset，再 provision Milvus collection（失败回滚）。
- **Dataset**：workspace 内的逻辑分组（如 `products`、`faq`），共享同一个 collection，靠标量字段 `dataset` 区分。每个 db workspace 自带一个 `default` dataset，**不可删除**（它是省略 `dataset` 时的隐式兜底）。
- **Record**：一行数据。必填 `text`（同时建 BM25 索引），可选 `id` / `category` / `tags` / `meta`；向量由服务端对 `text` 计算。物理主键 `{dataset}:{id}` 由服务端内部组装，**不出现在 wire 上**。

---

## 5. 数据模型（wire 字段）

> 标 `?` 的字段可能不出现（`null` 或被省略）。时间格式见 §1。

### Workspace
| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | string | workspace id（UUID） |
| `account_id` | string | 所属账号 |
| `name` | string | 名称 |
| `status` | enum | `active` \| `archived` |
| `kind` | enum | `fs` \| `db` |
| `app_id`? | string\|null | 治理标签 |
| `description`? | string\|null | 描述 |
| `created_at` / `updated_at` | string | RFC3339 |

### Dataset
| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | string | dataset id（UUID） |
| `workspace_id` | string | 所属 workspace |
| `name` | string | dataset 名 |
| `status` | enum | `active` \| `archived` |
| `description`? | string\|null | 描述 |
| `created_at` / `updated_at` | string | RFC3339 |

### WorkspaceKey（`GET /v1/workspaces/{id}/keys` 的列表项）
| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | string | key id（撤销时用） |
| `workspace_id` | string | 绑定的 workspace |
| `account_id` | string | 所属账号（建 key 时从 workspace 冗余，供鉴权单查） |
| `name` | string | key 名称 |
| `permission` | enum | `read` \| `readwrite` |
| `status` | enum | `active` \| `revoked` |
| `kind` | enum | `fs` \| `db`（建 key 时从 workspace 冗余，不可变） |
| `created_at` | string | RFC3339 |

> 明文 `wk_` 仅在创建响应里出现一次（字段 `key`）；列表接口只回元数据，不含明文。

### VectorSearchHit（`/v1/vectors/search` 命中项）
| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | string | 记录 id（不含 dataset 前缀） |
| `dataset` | string | 所属 dataset |
| `category` | string | 分类 |
| `tags` | string[] | 标签 |
| `text` | string | 原文 |
| `meta` | object | 自定义 JSON（技术上接受任意 JSON value；v0 建议用 object —— filter 仅能按 `meta.<key>` 过滤，非 object 无法被过滤） |
| `created_at` / `updated_at` | int64 | 毫秒 epoch |
| `score` | float | 相关性分数，**越大越相关**；含义由 `score_type` 决定 |
| `score_type` | string | `cosine`（语义 ANN，~[0,1]）/ `bm25`（全文，~[0,30+]）/ `rrf`（hybrid 融合，~[0,0.033]）。**跨 type 不可比**，读分数前先看本字段 |

### VectorRecordHit（`/v1/vectors/query` 命中项）
同 `VectorSearchHit`，但**没有 `score`**（直接按 id 查，非排序匹配）。

### PaginatedResponse&lt;T&gt;（列表接口）
| 字段 | 类型 | 说明 |
|---|---|---|
| `items` | T[] | 当前页 |
| `has_more` | bool | 是否还有下一页 |
| `next_cursor`? | string | 下页 `after` 游标；`has_more=false` 时不出现 |

---

## 6. 端点参考

汇总（🔑 `vk_`=控制面账号鉴权，🟦 `wk_`=数据面 workspace 鉴权，🔓=无需鉴权）：

| 方法 | 路径 | 鉴权 | 用途 |
|---|---|:--:|---|
| POST | `/v1/accounts` | 🔓 | 注册账号 |
| POST | `/v1/accounts/login` | 🔓 | 登录换新 `vk_` |
| POST | `/v1/accounts/anonymous` | 🔓 | 匿名 onboarding（建 **fs** workspace，db 勿用） |
| POST | `/v1/accounts/claim` | 🔑 | 认领匿名账号 |
| POST | `/v1/workspaces` | 🔑 | 建 workspace（`kind:"db"`） |
| GET | `/v1/workspaces` | 🔑 | 列 workspace（分页） |
| DELETE | `/v1/workspaces/{id}` | 🔑 | 软删 workspace（级联吊销全部 `wk_`） |
| POST | `/v1/workspaces/{id}/keys` | 🔑 | 签发数据面 `wk_`（`read` / `readwrite`） |
| GET | `/v1/workspaces/{id}/keys` | 🔑 | 列 `wk_` 元数据（无明文） |
| DELETE | `/v1/workspaces/{id}/keys/{key_id}` | 🔑 | 撤销 `wk_` |
| POST | `/v1/workspaces/{ws}/datasets` | 🔑 | 建 dataset |
| GET | `/v1/workspaces/{ws}/datasets` | 🔑 | 列 dataset（分页） |
| DELETE | `/v1/workspaces/{ws}/datasets/{name}` | 🔑 | 软删 dataset（不能删 `default`） |
| POST | `/v1/vectors/upsert` | 🟦 | 写入 / 覆盖记录（read-only `wk_` → 403） |
| POST | `/v1/vectors/search` | 🟦 | 向量检索 |
| POST | `/v1/vectors/query` | 🟦 | 按 id 查 |
| POST | `/v1/vectors/delete` | 🟦 | 按 id 删（read-only `wk_` → 403） |
| POST | `/admin/v1/tokens` | 🔑 | 铸账号级 scoped `vk_` 服务令牌 |
| POST | `/admin/v1/tokens/{id}/disable` | 🔑 | 撤销令牌 |

> 平台接入按 `app_id` 走 `/v1/apps/{app_id}/...`（鉴权外移，见平台管理 API 文档）；上表 `vk_` 控制面是当前直连形态。

---

### 6.1 账号

#### POST `/v1/accounts` 🔓
注册命名账号。两种**互斥**模式：email+password（console / CLI 自助），或 `{ name, app_id }` 平台无密码建号（无密码即不可 login，`vk_` 仅在本次响应返回一次；平台接入见 §2 末尾的平台说明）。
- 请求：`{ "name": string, "email": string, "password": string }` 或 `{ "name": string, "app_id": string }`（混填两种模式的字段 → `400 INVALID_INPUT`）
- 响应 200：`{ "account_id": string, "api_key": "vk_..." }`
- 错误：`409 ALREADY_EXISTS`（邮箱已注册 / `app_id` 已占用）。

#### POST `/v1/accounts/login` 🔓
邮箱密码登录，铸一把新的 `vk_`（name=`login`）。**会撤销该账号此前所有 `login` key**。
- 请求：`{ "email": string, "password": string }`
- 响应 200：`{ "account_id": string, "api_key": "vk_..." }`
- 错误：`401 UNAUTHORIZED`（账号不存在 / 密码错 / 账号被停用，统一同一错误，不泄露存在性）。

#### POST `/v1/accounts/anonymous` 🔓
零输入 onboarding，一次返回账号 key + workspace key。**注意建的 workspace 是 `kind=fs`**，db 场景请勿使用。
- 请求：无
- 响应 200：`{ "account_id", "api_key": "vk_...", "workspace_id", "workspace_key": "wk_..." }`

#### POST `/v1/accounts/claim` 🔑
把当前匿名账号升级为命名账号（补 email + 密码），原 `vk_` 继续有效。
- 请求：`{ "email": string, "password": string, "name"?: string }`
- 响应 200：`{ "account_id": string }`
- 错误：`400 INVALID_INPUT`（已认领 / 字段为空 / app_id 账号不可认领）、`409 ALREADY_EXISTS`（邮箱被占）。

---

### 6.2 Workspace（控制面，🔑 `vk_`）

#### POST `/v1/workspaces` 🔑
建 workspace。`kind:"db"` 时服务端额外 bootstrap `default` dataset + Milvus collection（单事务，provision 失败自动回滚，不留僵尸 workspace）。
- 请求：`{ "name": string, "kind"?: "fs"|"db", "description"?: string }`（`kind` 省略默认 `fs`，**做向量服务必须显式 `"db"`**）
- 响应 200：`Workspace` 对象
- 错误：`409 ALREADY_EXISTS`（同账号下同名 workspace）、`500 INTERNAL`（Milvus provision 失败，已回滚）。

#### GET `/v1/workspaces` 🔑
列出账号下 workspace（含 fs 与 db），分页（见 §12）。
- Query：`limit`?（默认 100，最大 200）、`after`?（游标）
- 响应 200：`PaginatedResponse<Workspace>`

#### DELETE `/v1/workspaces/{id}` 🔑
软删（`status=archived`），并在**同一事务内级联吊销该 workspace 的全部 `wk_`**。归档后：
- 数据面 `/v1/vectors/*` 调用返回 **`401 UNAUTHORIZED`**（不是 404）——`wk_` 鉴权不读 workspace 状态，归档对数据面的生效方式就是级联吊销 key；
- dataset 控制面（`/v1/workspaces/{ws}/datasets*`）仍返回 **`404 NOT_FOUND`**（加载 workspace 时校验其状态）。

- 响应 **200**：`{ "success": true, "data": null }`
- 错误：`403 PERMISSION_DENIED`（非本账号）、`404 NOT_FOUND`。
- ⚠️ 当前为软删，**不回收 Milvus 向量**（存储泄漏，见 followups H1）；唯一约束仍在，名称暂不可复用。

---

### 6.3 Workspace Key（控制面，🔑 `vk_`）

> 数据面凭证 `wk_` 的生命周期。明文只在创建时返回一次。

#### POST `/v1/workspaces/{id}/keys` 🔑
为 workspace 签发一把数据面 `wk_`。
- 请求：`{ "name"?: string, "permission"?: "read"|"readwrite" }`（`permission` 默认 `readwrite`）
- 响应 200：`{ "key": "wk_...", "permission": "read"|"readwrite" }` —— **明文仅此一次**。
- 错误：`400 INVALID_INPUT`（`permission` 非法）、`403 PERMISSION_DENIED` / `404 NOT_FOUND`（workspace 非本账号 / 不存在 / **已归档**——归档 workspace 不可再签发 key）。

#### GET `/v1/workspaces/{id}/keys` 🔑
列出该 workspace 的 key（仅元数据，无明文）。
- 响应 200：`WorkspaceKey[]`（直接数组，**不分页**）。

#### DELETE `/v1/workspaces/{id}/keys/{key_id}` 🔑
撤销 key（`status=revoked`，立即失效）。
- 响应 **204**：无响应体
- 错误：`404 NOT_FOUND`（key 不存在或不属于该 workspace）。

---

### 6.4 Dataset（控制面，🔑 `vk_`）

> 三个端点都要求目标 workspace 必须是 `kind=db` 且 active，否则 `400 WORKSPACE_KIND_MISMATCH` / `404`。

#### POST `/v1/workspaces/{ws}/datasets` 🔑
- 请求：`{ "name": string, "description"?: string }`（`name` charset `[a-zA-Z0-9_-]+`，≤64 字节，不含 `:`）
- 响应 **201**：`Dataset` 对象
- 错误：`409 ALREADY_EXISTS`（同名，大小写不敏感）、`400 INVALID_INPUT`。

#### GET `/v1/workspaces/{ws}/datasets` 🔑
列出 active dataset，分页。
- Query：`limit`? / `after`?
- 响应 200：`PaginatedResponse<Dataset>`

#### DELETE `/v1/workspaces/{ws}/datasets/{name}` 🔑
软删 dataset（`status=archived`）。
- 响应 **204**：无响应体
- 错误：`400 CANNOT_DELETE_DEFAULT_DATASET`（`default` 不可删，大小写不敏感）、`404 NOT_FOUND`。
- ⚠️ 软删不回收 Milvus 向量（同 followups H1）。

---

### 6.5 向量数据面（🟦 `wk_`）

> 四个端点的目标 workspace 由 `wk_` 绑定，请求体**不带 `workspace_id`**。公共参数仅 `dataset`?（省略取 `default`）。`upsert` / `delete` 需 `readwrite` 权限的 `wk_`（read-only → `403 PERMISSION_DENIED`）。

#### POST `/v1/vectors/upsert` 🟦
按 `(dataset, id)` 插入或整行替换。单次最多 **500** 条。
- 请求：
```json
{
  "dataset": "products",
  "write_mode": "upsert",
  "records": [
    {
      "id": "sku-1",
      "text": "Air Jordan 1",
      "category": "shoes",
      "tags": ["sale", "new"],
      "meta": { "price": 1299 }
    }
  ]
}
```
  每条 record 除 `text` 外都有默认值：`id`→服务端 UUID（**insert-only，非幂等**，见 §11）、`category`→`"default"`、`tags`→`[]`、`meta`→`{}`。
  顶层可选 `write_mode`：`"upsert"`（默认，幂等安全）| `"insert"`（跳过查重，~3x 速，**调用方必须保证 pk 唯一**；重复 pk 是 Milvus 未定义行为，见 §11）。
- 响应 200：
```json
{ "success": true, "data": { "ids": ["sku-1"], "commit_ts": 1735689600000 } }
```
  - `ids`：实际写入的 id，**按请求顺序、且已对同批重复 id 去重**（last-wins，见 §11），所以可能比 `records` 短。省略 `id` 的记录在这里**首次也是唯一一次**暴露服务端生成的 UUID，务必客户端留存。
  - `commit_ts`：服务端写完时刻（毫秒 epoch）。Milvus REST 不返回真 commit ts，此值用于同机 read-your-writes 足够。
- 错误：`400 INVALID_INPUT`（字段校验，含 **text/meta/tags/category/id/dataset 等单字段长度或字符集超限**，`error` 形如 `<field>: <reason>`）、`403 PERMISSION_DENIED`（read-only `wk_`）、`413 PAYLOAD_TOO_LARGE`（**仅** `records` 条数 >500）、`500 EMBEDDING_FAILED`。

#### POST `/v1/vectors/search` 🟦
检索目标 dataset，**隐式锁定**（v0 不支持跨 dataset）。`mode` 选择 ranker：

| `mode` | 行为 | 嵌入 query？ | `score_type` | 分数范围 |
|---|---|---|---|---|
| `hybrid`（**默认**） | 稠密 ANN + BM25，RRF 融合 | 是 | `rrf` | ~[0, 0.033] |
| `semantic` | 对嵌入后的 query 做稠密 ANN | 是 | `cosine` | ~[0, 1] |
| `fulltext` | 对分词后的 `text` 做 BM25 全文 | 否 | `bm25` | ~[0, 30+] |

分数**跨 mode 不可比**，读分数前先看 `score_type`。`fulltext` 完全跳过嵌入调用（更便宜，也是唯一不依赖 embedding model 的 mode）。`hybrid` 失败直接报错，**不静默降级到 semantic**。
- 请求：
```json
{
  "dataset": "products",
  "query": "sneakers under 1500",
  "mode": "semantic",
  "top_k": 10,
  "min_score": 0.4,
  "filter": {
    "must": [
      { "field": "meta.price", "op": "lt", "value": 1500 },
      { "field": "meta.category", "op": "eq", "value": "shoes" }
    ]
  }
}
```
  `mode` 可选，默认 `hybrid`（可选 `semantic` / `fulltext`）；`top_k` 默认 10，最大 100；`filter` 可选，见 §7。
  `min_score` 可选，**相关度下限**：丢掉低于它的命中。**仅 `semantic`(cosine)/`fulltext`(bm25) 生效**；`hybrid`（含默认 mode）传入即 `400`——RRF 是排名不是相关度，要门槛请用 `top_k` 或显式 `mode=semantic`。在 `top_k` 之后裁剪，故结果可能 **少于 `top_k`**（要更多过线就调大 `top_k`）。需按模型校准：dense 下无关文本也 ~0.15–0.25，有效的 cosine 门槛要明显高于此（如 0.4–0.6），没有通用的"0.5"。
- 响应 200：`{ "hits": VectorSearchHit[] }`（每个 hit 含 `score` + `score_type`）。
- 错误：`400 INVALID_INPUT`（query 为空或 >65535 字节 / top_k=0 / filter 非法 / `min_score` 非有限值或与 `mode=hybrid` 同用）、`413 PAYLOAD_TOO_LARGE`（top_k>100）、`500 EMBEDDING_FAILED`（semantic/hybrid 嵌入失败）/ `500`（hybrid 后端失败，不降级）。

#### POST `/v1/vectors/query` 🟦
按 id 直查。**不保证顺序；不存在的 id 静默跳过（不报错）**。单次最多 **500** 个 id。
- 请求：`{ "dataset": "products", "ids": ["sku-1","sku-2"] }`
- 响应 200：`{ "hits": VectorRecordHit[] }`
- 错误：`400 INVALID_INPUT`（ids 为空）、`413 PAYLOAD_TOO_LARGE`（>500）。

#### POST `/v1/vectors/delete` 🟦
按 id 硬删。单次最多 **500** 个 id。
- 请求：`{ "dataset": "products", "ids": ["sku-1","sku-2"] }`
- 响应 200：`{ "delete_count": 2 }`
- ⚠️ `delete_count` 是 Milvus 创建的 **tombstone 数 = `len(ids)`**，与「实际存在并被删的行数」**无关**。要区分请先 `query`。
- 错误：`400 INVALID_INPUT`（ids 为空）、`403 PERMISSION_DENIED`（read-only `wk_`）、`413 PAYLOAD_TOO_LARGE`（>500）。

---

### 6.6 服务令牌（控制面，🔑 `vk_`）

#### POST `/admin/v1/tokens` 🔑
铸一把账号级 scoped `vk_` 服务令牌，归属调用者账号。v0 无独立 admin 网关：任何持账号 `vk_` 者都可为**自己的**账号铸令牌。**注意：这是控制面 `vk_`，不是数据面 `wk_`**——数据面凭证请用 §6.3 的 `wk_`。
- 请求：
```json
{
  "app_id": "search-svc",
  "name": "prod",
  "allowed_workspaces": ["<ws_id>"],
  "expires_at": 1767225600000
}
```
  `allowed_workspaces`? 省略=账号内不限；列表里每个 ws 都会校验归属本账号。`expires_at`? 为毫秒 epoch，省略=永不过期。
- 响应 **201**：`{ "id": string, "token": "vk_..." }` —— `token` **仅此一次返回**，之后无法再取。
- 错误：`400 INVALID_INPUT`（`app_id`/`name` 空、`expires_at` 非法）、`403 PERMISSION_DENIED`（`allowed_workspaces` 含他人 workspace）、`404 NOT_FOUND`。

#### POST `/admin/v1/tokens/{id}/disable` 🔑
撤销令牌（先校验归属本账号）。
- 响应 **204**：无响应体
- 错误：`404 NOT_FOUND`（不存在 **或** 属于他账号——不泄露存在性）。

---

## 7. Filter DSL（v0）

Qdrant 风格的严格子集，仅用于 `/v1/vectors/search` 的 `filter`：

- 只有 `must`（无 `should` / `must_not`），所有 clause **AND** 组合，并与基础 scope（`dataset == "X" && status == "active"`）合并。
- `field` 只能是 `meta.<顶层key>`（不支持嵌套）。平台字段（`dataset`、`tags`、`status` 等）**不可经此 DSL 过滤**——`dataset` 已是基础 scope，其余未开放。
- `op`：`eq` \| `in` \| `gt` \| `gte` \| `lt` \| `lte`。
- `value`：`eq` 接受标量 string/number/bool；范围 `gt`/`gte`/`lt`/`lte` **仅** number/string（bool/null/数组/对象会 400）；`in` 为标量数组，**非空且 ≤100 项**（解析期展开为 OR 链）。

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
| `dataset` | ≤ 64 字节 |
| `id` | ≤ 64 字节（且 `{dataset}:{id}` 合成后 ≤ 128 字节） |
| `text` | 非空，≤ 65535 字节（UTF-8，Milvus VARCHAR 硬上限）；更大需客户端分片 |
| `meta` | JSON 序列化后 ≤ 16 KB |
| `tags` | ≤ 8 个，单个 ≤ 128 字节，不可空串 |
| `category` | 非空，≤ 64 字节 |
| `upsert.records` | 非空，≤ 500 / 次 |
| `query.ids` / `delete.ids` | 非空，≤ 500 / 次 |
| `search.top_k` | 默认 10，最大 100 |
| `filter` `meta.<key>` | key 须 `[a-zA-Z0-9_-]+`，不支持嵌套 |
| `filter` `in` 数组 | 非空，≤ 100 |
| 列表 `limit` | 默认 100，最大 200（超出静默截断） |

所有字段校验在任何 Milvus 写入**之前**完成。**单字段超限（长度 / 字符集 / 非空）返回 `400 INVALID_INPUT`**（`error` 形如 `<field>: <reason>`）；**仅批量条数超限（`records` / `ids` >500、`top_k` >100）返回 `413 PAYLOAD_TOO_LARGE`**。

---

## 9. 不属于本 API 的端点

以下端点属于 **fs workspace**（个人知识库 / FUSE）能力面，服务端强制目标 workspace 为 `kind=fs`——用 db workspace 的 `wk_` 调用会 `400 WORKSPACE_KIND_MISMATCH`，不要在 db SDK 里暴露：

- `POST /v1/search`、`POST /v1/grep`、`POST /v1/sql`、`GET /v1/abstract/{path}`、`GET /v1/overview/{path}`、`/v1/fs/*`、`/v1/events`、`/v1/collections/*`

它们与向量服务是两条独立产品线。fs 数据面同样以 `wk_` 鉴权，但绑定的是 `kind=fs` 的 workspace。

---

## 10. 错误码

失败响应固定为 `{ "success": false, "error_code": "...", "error": "..." }`。**只匹配 `error_code`**。

| `error_code` | HTTP | 含义 |
|---|---:|---|
| `INVALID_INPUT` | 400 | 通用校验失败（字符集 / 长度 / 缺字段）。`error` 携带 `<field>: <reason>` |
| `WORKSPACE_KIND_MISMATCH` | 400 | 向量 API 打到了 fs workspace（或反之） |
| `CANNOT_DELETE_DEFAULT_DATASET` | 400 | 拒绝删除 `default` dataset |
| `UNAUTHORIZED` | 401 | 缺失 / 无效 / 过期 / 用错面的 bearer token（如 `wk_` 打控制面、`vk_` 打数据面） |
| `PERMISSION_DENIED` | 403 | read-only `wk_` 调 `upsert`/`delete`；或 `vk_` 操作了他账号资源 |
| `NOT_FOUND` | 404 | workspace / dataset / key / token 不存在或已归档 |
| `ALREADY_EXISTS` | 409 | workspace / dataset 同名冲突（大小写不敏感）/ 邮箱已注册 |
| `PAYLOAD_TOO_LARGE` | 413 | **仅**批量条数超限（`records` / `ids` >500、`top_k` >100）；单字段长度超限走 `INVALID_INPUT` |
| `QUOTA_EXCEEDED` | 429 | 向量 API 当前不返回；保留 |
| `EMBEDDING_FAILED` | 500 | 服务端嵌入上游错误 |
| `INTERNAL` | 500 | 存储 / 死锁 / 未预期错误的兜底，故意不透出细节 |

---

## 11. 幂等性与重试

**写入语义由 `write_mode`（默认 `upsert`）+ 是否自带 `id` 共同决定：**

| `write_mode` | `id` | 行为 | 幂等 |
|---|---|---|---|
| `upsert`（默认） | 自带 | 按 `(workspace,dataset,id)` 原地整行替换；`created_at`/`updated_at` 重置，`meta`/`tags`/`text` 全量覆盖 | ✅ |
| `upsert`（默认） | 省略 | 服务端 UUID → 内部走 **insert** 快路径（UUID 不可能撞） | ❌（每次新 UUID） |
| `insert` | 自带 | 直接 insert、**跳过查重**，~3x 速；**调用方保证 `id` 唯一** | ❌ |
| `insert` | 省略 | 服务端 UUID → insert | ❌ |

> **`write_mode=insert` 拿安全换速度**：Milvus insert 不检查 pk 唯一性，重复 `pk` 是 **Milvus 未定义行为**——物理累积多行且 compaction **不自动清**（纯 insert 无 tombstone，要后续 delete/upsert 才会清，膨胀持久），`query`/`search` 返回哪条 unknown。仅用于 id 天然唯一的场景（自增 / UUID / 一次性导入）。**怕重试或会重导的管道必须用默认 `upsert`**——只有它有确定的「最新覆盖」（insert 新 + delete 旧）。

**同一次 upsert 内重复 `id`**：服务端去重，**last-wins**（取最后一次出现的值，但保留该 id 首次出现的位置），重复项在嵌入前丢弃。无报错，响应 `ids` 反映去重后结果，可能短于请求 `records`。

**delete**：`delete_count` 是 tombstone 数 = `len(ids)`，不代表实际删除行数（见 §6.5）。

**commit_ts**：服务端本地时间近似，仅用于同机 read-your-writes，不是分布式可比的逻辑时钟。

---

## 12. 分页

`GET /v1/workspaces` 与 `GET /v1/workspaces/{ws}/datasets` 用游标分页（`GET /v1/workspaces/{id}/keys` 不分页，直接返数组）：

- `limit`：每页条数，默认 100，最大 200（超出静默截断）。
- `after`：上一页 `next_cursor`，不透明字符串。

```json
{ "success": true, "data": {
  "items": [ /* ... */ ],
  "has_more": true,
  "next_cursor": "<把它作为下次 after 传入>"
} }
```

`has_more=false` 时 `next_cursor` 不出现。排序稳定但实现定义（当前按行 id 升序，UUID 字典序），**非业务有意义排序**；需要特定排序请取全量后客户端再排。

---

## 13. 附录：运维端点

非业务方核心，供探活 / 监控：

| 方法 | 路径 | 鉴权 | 说明 |
|---|---|:--:|---|
| GET | `/healthz` | 🔓 | 存活探针，恒返回 `ok`（纯文本，不查后端） |
| GET | `/v1/ready` | 🔓 | 就绪探针，检查 MySQL + Milvus；就绪 200 / 否则 503，体含各组件状态 |
| GET | `/capabilities` | 🔓 | 能力位（如 `summary_enabled`，fs 相关） |
| GET | `/install.sh` | 🔓 | CLI 安装脚本（构建期内嵌，随服务端版本更新） |
| GET | `/v1/metrics` | 单独 token | Prometheus 文本格式；未配置 token 或 token 错均返回 404 |
| POST | `/admin/v1/reconcile/{workspace_id}` | 单独 token | 按 workspace 触发 MySQL↔Milvus 对账。`?dry_run=true\|false`，**默认 `true`（只报告漂移不修复）**，显式 `false` 才入队修复 / 删孤儿；鉴权复用 `/v1/metrics` 的 token，未配置或 token 错均 404 |

`/healthz`（存活）与 `/v1/ready`（就绪）分离：systemd watchdog / k8s livenessProbe 应打 `/healthz`，避免后端瞬时抖动触发无意义重启。
