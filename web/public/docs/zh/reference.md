# 详细文档

这是 Veda 的完整技术参考：架构、认证、两条数据面、错误码、运维端点、以及该知道的边界。想快速跑通看 [快速开始](#/docs/quickstart)；只关心向量库看 [向量库 API](#/docs/vectors)；这里是把所有东西放在一起、能当手册查的那一份。

---

## 1. Veda 是什么

一句话：**把文件、向量检索、SQL 查询放在一套 API 后面的知识存储服务**。后端是 MySQL（控制面：元数据、账号、任务队列）+ Milvus（数据面：向量、全文、结构化数据）+ 一个异步 worker（嵌入、摘要）。

它有两种 workspace，建库时选定、之后不可改，决定它走哪条数据面：

| | 文件库 File Workspace | 向量库 Vector Workspace |
|---|---|---|
| `kind` | `fs`（默认） | `db` |
| 数据模型 | 文件 / 目录 | 向量记录（text + meta） |
| 数据面 | `/v1/fs/*`、`/v1/search`、`/v1/grep`、`/v1/sql`、`/v1/abstract`、`/v1/overview`、`/v1/collections/*`、`/v1/events`、FUSE | `/v1/vectors/{upsert,search,query,delete}` |
| 接入 | CLI / FUSE / HTTP | REST API / SDK |
| 典型场景 | 个人知识库、Agent 记忆、代码搜索 | 业务应用的托管向量检索 |

两条线**互不相通**：文件库的 `wk_` 不能打向量端点，反之亦然（会被 `400 WORKSPACE_KIND_MISMATCH` 挡）。它们是两条独立产品线，共用同一套账号、认证和运维。

**心智模型**：一个**账号（Account）**下挂多个 **workspace**；每个 workspace 是一个隔离边界。控制面（建库、签 key、管 dataset）用账号级 `vk_`；数据面（读写数据）用 workspace 级 `wk_`。

---

## 2. 认证模型（先读这节）

凭证一律走 `Authorization: Bearer <token>`。两种 key，两个面，别混用：

| Key | 前缀 | 面 | 绑定 | 用途 |
|---|---|---|---|---|
| **账号 key** | `vk_` | 控制面 | 整个账号 | 建/删 workspace、管 dataset、签发 `wk_`、铸服务令牌 |
| **workspace key** | `wk_` | 数据面 | 单个 workspace | 读写该 workspace 的数据（fs 和 db 通用） |

规则：

- **`wk_` 绑定单个 workspace**。所以数据面请求体**不带 `workspace_id`**——目标由 key 决定。fs 用内部 `AuthWorkspace`、db 用 `AuthDbWorkspace`，各自校验 `kind`，不匹配 `400 WORKSPACE_KIND_MISMATCH`。
- **`wk_` 分读写**：`read` / `readwrite`。只读 key 能 search / query / 读文件，不能 upsert / delete / 写文件（→ `403 PERMISSION_DENIED`）。
- **`vk_` 只在控制面**，由平台 / 控制台持有，**不下发给业务方**。业务方通常只拿一把 `wk_`。
- **用错面 = 401**：`vk_` 打数据面、或 `wk_` 打控制面，都按无效凭证 `401 UNAUTHORIZED` 拒绝。
- **吊销立即生效**：archive workspace 会在同一事务里把它名下所有 `wk_` 置为 `revoked`；suspend 账号会让其名下所有 key（`vk_`+`wk_`）在下次请求即失效。
- **JWT 已彻底移除**：没有 `POST /v1/workspaces/{id}/token`，没有 `jwt_secret`，鉴权全是纯 key 校验（key 以 SHA-256 哈希存储）。
- **身份自省**：`GET /v1/whoami`（Bearer `wk_`，fs/db 通用、不校验 kind）返回该 key 所属的 `{workspace_id, kind, permission}`。手上只有一把裸 `wk_` 想知道它指向哪个 workspace 时用这个——CLI `veda status` / `veda init --import-key wk_…` 靠它回填本地配置里的 workspace id。

> `vk_` 是账号根权限，没有能力分级。要限制一把令牌只能动某几个 workspace，用 `POST /admin/v1/tokens` 的 `allowed_workspaces` scope（见 §4.5）。

### 平台接入（AI Platform）

公司 AI Platform 走另一套按 `app_id` 的控制面 `/v1/apps/{app_id}/*`：鉴权外移到平台网关，veda 信任路径里的 `app_id`、首次访问自动开户。业务方在这套模型里同样只拿 `wk_`。详见 §4.6。

---

## 3. 全局约定

### Base URL

`<BASE>` 由部署决定。公司部署示例：`https://veda.ddmc-inc.com`。server 默认监听 `0.0.0.0:3000`。下文示例统一用 `$BASE`。业务路径都在 `/v1/*` 或 `/admin/v1/*` 下。

### 响应信封

除少数例外，所有有响应体的接口统一用 `ApiResponse<T>`：

```json
// 成功
{ "success": true, "data": { /* T，随接口而定 */ } }
// 失败
{ "success": false, "error_code": "INVALID_INPUT", "error": "text: must not be empty" }
```

- `error_code` 是**稳定的机器可读码**，只在失败时出现。**客户端只匹配 `error_code`，不要解析 `error` 文案**（文案随时可能改，部分内部错误的文案还会被抹成 `internal server error`）。
- 成功响应没有 `error_code` / `error`；失败响应没有 `data`。
- **例外**：部分删除接口返回 `204 No Content`（无信封）；`/v1/events` 是裸 SSE 流；运维端点（`/healthz`、`/v1/ready`、`/v1/metrics`）各有自己的格式。

### HTTP 状态码

成功用 `200`（一般）/ `201`（建 dataset、铸 token、apps 面建 workspace）/ `204`（删 dataset、撤 key、禁用 token）。失败时状态码与 `error_code` 一一对应，见 §7。

> ⚠️ 状态码有个不对称：`vk_` 直连建 workspace（`POST /v1/workspaces`）返 **200**，但 apps 平台面建 workspace（`POST /v1/apps/{app_id}/workspaces`）返 **201**。同时打两套面的 SDK 要特判。

### 时间格式（两种并存）

| 出现位置 | 类型 | 示例 |
|---|---|---|
| 控制面对象（Workspace / Dataset / Key 的 `created_at` / `updated_at`） | **RFC3339 字符串** | `"2026-05-29T12:34:56Z"` |
| 向量 hit 的 `created_at` / `updated_at`、`upsert` 的 `commit_ts`、铸 token 请求的 `expires_at` | **int64 毫秒 epoch** | `1735689600000` |

生成类型时别把这两类统一。

### 分页

`GET /v1/workspaces`、`GET /v1/workspaces/{ws}/datasets` 用游标分页：

```json
{ "success": true, "data": {
  "items": [ /* ... */ ], "has_more": true, "next_cursor": "<下次 after 传它>"
} }
```

- `limit`：每页条数，默认 100，最大 200（超出静默截断）。
- `after`：上一页的 `next_cursor`，不透明字符串。`has_more=false` 时 `next_cursor` 不出现。
- 排序稳定但实现定义（按行 id 升序，UUID 字典序），**非业务有意义排序**。需要特定排序请取全量后客户端再排。
- 注意：`GET /v1/workspaces/{id}/keys` **不分页**，直接返数组。

---

## 4. 控制面 API（🔑 `vk_`）

汇总（🔑 `vk_` 账号鉴权 · 🔓 无需鉴权 · 🏢 apps 平台面无 veda 凭据 · 🛠 ops `metrics_token`）：

| 方法 | 路径 | 鉴权 | 成功码 | 用途 |
|---|---|:--:|:--:|---|
| POST | `/v1/accounts` | 🔓 | 200 | 注册账号（email 模式 / app_id 模式） |
| POST | `/v1/accounts/anonymous` | 🔓 | 200 | 匿名 onboarding（建 **fs** 库） |
| POST | `/v1/accounts/claim` | 🔑 | 200 | 认领匿名账号 |
| POST | `/v1/accounts/login` | 🔓 | 200 | 邮箱登录换新 `vk_` |
| POST | `/v1/workspaces` | 🔑 | 200 | 建 workspace |
| GET | `/v1/workspaces` | 🔑 | 200 | 列 workspace（分页） |
| DELETE | `/v1/workspaces/{id}` | 🔑 | 200 | 软删 workspace |
| POST | `/v1/workspaces/{id}/keys` | 🔑 | 200 | 签发数据面 `wk_` |
| GET | `/v1/workspaces/{id}/keys` | 🔑 | 200 | 列 `wk_` 元数据（不分页） |
| DELETE | `/v1/workspaces/{id}/keys/{key_id}` | 🔑 | 204 | 撤销 `wk_` |
| POST | `/v1/workspaces/{ws}/datasets` | 🔑 | 201 | 建 dataset |
| GET | `/v1/workspaces/{ws}/datasets` | 🔑 | 200 | 列 dataset（分页） |
| DELETE | `/v1/workspaces/{ws}/datasets/{name}` | 🔑 | 204 | 软删 dataset（不能删 `default`） |
| POST | `/admin/v1/tokens` | 🔑 | 201 | 铸账号级 scoped `vk_` 服务令牌 |
| POST | `/admin/v1/tokens/{id}/disable` | 🔑 | 204 | 撤销令牌 |
| POST | `/v1/apps/{app_id}/workspaces` | 🏢 | 201 | 平台面建 workspace |
| GET | `/v1/apps/{app_id}/workspaces` | 🏢 | 200 | 平台面列 workspace |
| DELETE | `/v1/apps/{app_id}/workspaces/{id}` | 🏢 | 200 | 平台面删 workspace |

### 4.1 账号

**POST `/v1/accounts`** 🔓 — 两种互斥模式：
- **email 模式**（console / CLI）：`{ name, email, password }`，返回 `{ account_id, api_key: "vk_…" }`。邮箱已注册 → `409 ALREADY_EXISTS`。
- **app_id 模式**（平台）：`{ name, app_id }`（**不带 email/password**），返回 `{ account_id, api_key: "vk_…", app_id }`。`app_id` 唯一，冲突 → `409`。
- 混合输入（app_id + email/password）或两者都不给 → `400 INVALID_INPUT`。

> ⚠️ v0 这个端点是**公开**的，任何人能抢注任意 `app_id`。仅限可信内网；上公网前必须加平台凭据。

**POST `/v1/accounts/anonymous`** 🔓 — 零输入，一次返回 `{ account_id, api_key: "vk_…", workspace_id, workspace_key: "wk_…" }`。**注意建的是 `kind=fs`**，向量库用不了。

**POST `/v1/accounts/claim`** 🔑 — 把匿名账号升级成命名账号（补 email + 密码），原 `vk_` 继续有效。已认领 / app_id 账号 / 字段空 → `400`；邮箱被占 → `409`。

**POST `/v1/accounts/login`** 🔓 — 邮箱密码登录，铸一把新 `vk_`（name=`login`），并**撤销该账号此前所有 `login` key**。账号不存在 / 密码错 / 被停用统一返 `401`（不泄露存在性）。

### 4.2 Workspace

**POST `/v1/workspaces`** 🔑 — `{ name, kind?: "fs"|"db", description? }`（`kind` 省略默认 `fs`，**做向量库必须显式 `"db"`**）。`kind=db` 时服务端在单事务里建 workspace + `default` dataset，再 provision Milvus collection（失败回滚，不留僵尸）。返回完整 `Workspace` 对象（200）。同名 → `409`；Milvus provision 失败 → `500`（已回滚）。

**GET `/v1/workspaces`** 🔑 — 列账号下所有 workspace（fs+db），分页。

**DELETE `/v1/workspaces/{id}`** 🔑 — 软删（`status=archived`），同事务级联撤销名下 `wk_`。返回 **200** `{ "success": true, "data": null }`。非本账号 → `403`，不存在 → `404`。
> ⚠️ 当前是软删，**不回收 Milvus 向量**（存储泄漏），名称暂不可复用。

### 4.3 Workspace Key（数据面 `wk_` 的生命周期）

**POST `/v1/workspaces/{id}/keys`** 🔑 — `{ name?, permission?: "read"|"readwrite" }`（`permission` 默认 `readwrite`）。返回 `{ "key": "wk_…", "permission": "…" }`——**明文仅此一次**。归档的 workspace 不能再签 key（→ 404）。

**GET `/v1/workspaces/{id}/keys`** 🔑 — 返回 `WorkspaceKey[]`（**直接数组，不分页**，仅元数据无明文）。每项含 `id / workspace_id / account_id / name / permission / status / kind / created_at`。

**DELETE `/v1/workspaces/{id}/keys/{key_id}`** 🔑 — 撤销（`status=revoked`，立即失效）。返回 **204**。key 不属于该 workspace → `404`。

### 4.4 Dataset（仅 db workspace）

三个端点都要求目标 workspace 是 `kind=db` 且 active，否则 `400 WORKSPACE_KIND_MISMATCH` / `404`。

**POST `/v1/workspaces/{ws}/datasets`** 🔑 — `{ name, description? }`（`name` charset `[a-zA-Z0-9_-]+`、≤64 字节、不含 `:`）。返回 **201** `Dataset`。同名（大小写不敏感）→ `409`。

**GET `/v1/workspaces/{ws}/datasets`** 🔑 — 列 active dataset，分页。

**DELETE `/v1/workspaces/{ws}/datasets/{name}`** 🔑 — 软删（`status=archived`），返回 **204**。`default` 不可删（大小写不敏感）→ `400 CANNOT_DELETE_DEFAULT_DATASET`；不存在 → `404`。同样不回收 Milvus 向量。

### 4.5 服务令牌（scoped `vk_`）

**POST `/admin/v1/tokens`** 🔑 — 铸一把账号级 scoped `vk_`，归属调用者账号。v0 无独立 admin 网关，任何持账号 `vk_` 者都可为**自己的**账号铸令牌。**这是控制面 `vk_`，不是数据面 `wk_`**。

```json
{ "app_id": "search-svc", "name": "prod",
  "allowed_workspaces": ["<ws_id>"], "expires_at": 1767225600000 }
```

`allowed_workspaces`? 省略=账号内不限，列表里每个 ws 都校验归属本账号（外部 ws → `403`）。`expires_at`? 毫秒 epoch，省略=永不过期。返回 **201** `{ id, token: "vk_…" }`——`token` 仅此一次。

**POST `/admin/v1/tokens/{id}/disable`** 🔑 — 撤销（先校验归属）。返回 **204**。不存在或属他账号 → 都返 `404`（不泄露存在性）。

> `app_id` 在令牌上只是**治理标签**，不是安全边界；真正的隔离是 `allowed_workspaces` + `workspace.kind`。

### 4.6 apps 平台面（🏢 `/v1/apps/{app_id}/*`）

供公司 AI Platform 接入。这组端点**不读 `Authorization`**——平台网关已证明调用方可代表 `app_id`，veda 信任路径里的 `app_id` 并据此解析 / 自动开户。与上面的 `vk_` 直连面并存。

- **POST `/v1/apps/{app_id}/workspaces`** — 首次访问该 `app_id` 时**自动开户**（passwordless 账号，不铸 `vk_`），建 workspace（请求体的 `app_id` 被路径覆盖）。返回 **201** `Workspace`。
- **GET `/v1/apps/{app_id}/workspaces`** — 列该 app 的 workspace。未知 app_id → **空页**（不自动开户，GET 无副作用）。返回 200 分页。
- **DELETE `/v1/apps/{app_id}/workspaces/{id}`** — 软删。跨租户的不存在统一返 `404`。返回 **200**。
- 账号被 suspend → 这组端点统一 `401`（`account suspended`）。

app_id 账号是 passwordless 的：不能 login、不能 claim，`app_id` 与 `(email,password)` 在一个账号上互斥。

---

## 5. 向量库数据面（🟦 `wk_`）

四个端点，目标 workspace 由 `wk_` 绑定，请求体**不带 `workspace_id`**。公共参数仅 `dataset`?（省略取 `default`）。`upsert` / `delete` 需 `readwrite` 的 `wk_`（只读 → `403`）。

| 方法 | 路径 | 权限 | 用途 |
|---|---|---|---|
| POST | `/v1/vectors/upsert` | readwrite | 写入 / 覆盖记录，≤500/次 |
| POST | `/v1/vectors/search` | read | 向量检索（三模式） |
| POST | `/v1/vectors/query` | read | 按 id 直查，≤500/次 |
| POST | `/v1/vectors/delete` | readwrite | 按 id 删，≤500/次 |

**核心概念**：
- **Dataset**：workspace 内的逻辑分组（如 `products`、`faq`），共享同一 collection，靠标量字段 `dataset` 区分。每个 db workspace 自带 `default` dataset（不可删，省略 `dataset` 时的兜底）。
- **Record**：一行数据。必填 `text`（服务端据此算向量 + 建 BM25 索引），可选 `id` / `category` / `tags` / `meta`。物理主键 `{dataset}:{id}` 由服务端内部组装，不出现在 wire 上。

**写入语义（`write_mode`）**：顶层可选 `write_mode`：`"upsert"`（默认，幂等安全）/ `"insert"`（跳过查重、~3x 速、**调用方必须保证 pk 唯一**）。省略 `id` 的记录始终走 insert 快路径（UUID 不可能撞）。

**检索模式（`mode`）**：

| `mode` | 行为 | 嵌入 query？ | `score_type` | 分数范围 |
|---|---|:--:|---|---|
| `hybrid`（默认） | 稠密 ANN + BM25，RRF 融合 | 是 | `rrf` | ~[0, 0.033] |
| `semantic` | 对嵌入后的 query 做稠密 ANN | 是 | `cosine` | ~[0, 1] |
| `fulltext` | 对分词后的 `text` 做 BM25 | 否 | `bm25` | ~[0, 30+] |

分数**跨 mode 不可比**，读分数前先看 `score_type`。`hybrid` 失败直接报错，**不静默降级**。`min_score`（相关度下限）仅 `semantic`/`fulltext` 生效，与 `hybrid` 同用即 `400`。

> 向量库数据面的**完整字段、请求/响应示例、Filter DSL、限制、幂等性、错误码**见 [向量库 API](#/docs/vectors)。本节是产品视角的概览，那页是逐字段契约。

---

## 6. 文件库数据面（🟦 `wk_`，`kind=fs`）

所有 fs 端点用 `wk_` 鉴权（绑定 fs workspace），写操作需 `readwrite`。完整 CLI 见 [CLI 速查](#/docs/cli)，本地目录形态见 [FUSE 挂载](#/docs/fuse)。

### 文件 CRUD（`/v1/fs/*`，上传体上限 50MB）

| 方法 / 路径 | 说明 |
|---|---|
| `PUT /v1/fs/{path}` | 写原始字节。有效 UTF-8 按文本存储、分块和索引；非 UTF-8 原样存为 blob（PDF 额外抽取文本用于搜索，图片等二进制不索引）。支持 `If-Match: "<rev>"`（CAS，不匹配 `412`）、`If-None-Match: "<sha256>"`（内容相同则不重写，返回 `content_unchanged:true`）。返回 `{ file_id, revision, content_unchanged }` + `ETag`。路径是目录 → `409`；单文件上限 50MB。 |
| `POST /v1/fs/{path}` | 追加内容（无 CAS）。 |
| `GET /v1/fs/{path}` | 读原始字节并返回存储的 `Content-Type`。`?stat` 取元数据、`?list` 列目录、`?lines=start:end` 取行片段；`Range: bytes=a-b` 返回 `206`。无参数返回全文。 |
| `HEAD /v1/fs/{path}` | 取 `FileInfo`（path/file_id/is_dir/size/mime/revision/checksum/时间）。 |
| `DELETE /v1/fs/{path}` | 删（目录递归）。 |
| `POST /v1/fs-copy` | `{ from, to }` 服务端复制（内容寻址去重）。 |
| `POST /v1/fs-rename` | `{ from, to }` 重命名。 |
| `POST /v1/fs-mkdir` | `{ path }` 建目录。 |
| `POST /v1/grep` | `{ pattern, path_prefix?, ignore_case?, max_results?=100 }` 字面子串扫描（非正则，同步），返回 `{path, line_no, line}[]`。 |

> 删根 `DELETE /v1/fs` 恒返 `400`（禁止）。

### 搜索（`POST /v1/search`）

`{ query, mode?, limit?, path_prefix?, detail_level? }`。`mode` 默认 `hybrid`（同向量库三模式）；`limit` 默认 10、上限 100；`detail_level` 默认 `full`。返回 `SearchHit[]`，每个 hit 带 `score_type`（`rrf`/`bm25`/`cosine`，跨 type 不可比）。嵌入是**异步**的，刚写入的文件要等几秒才可搜。

### 三层信息模型（L0 / L1 / L2）

每个文件 / 目录自动生成分层摘要，按需取，省 token：

| 层 | 端点 | 大小 | 含义 |
|---|---|---|---|
| **L0 Abstract** | `GET /v1/abstract/{path}` 或 search `detail_level=abstract` | 一句话 | 一句话概括（文件和目录都进 Milvus，可被向量搜索命中） |
| **L1 Overview** | `GET /v1/overview/{path}` 或 `detail_level=overview` | ~2k token | 结构化概览 |
| **L2 Full** | `GET /v1/fs/{path}` 或 search `detail_level=full`（默认） | 全文 | 原文 chunk |

`/v1/abstract`、`/v1/overview` 是**三态响应**：`200`（已就绪）/ `202 + Retry-After:5`（生成中）/ `501 + Cache-Control:no-store`（server 未配 `[llm]`，摘要功能禁用）。摘要依赖可选的 LLM 配置，未配置时自动禁用。根 `/` 没有摘要（无根 dentry）。

### SQL（`POST /v1/sql`）

`{ sql }`，DataFusion 引擎，workspace 内作用域。表：`files`（递归 dentry）、每个结构化 collection 按名注册成表。内置 8 个 FS 标量 UDF（`veda_read/write/append/exists/size/mtime/remove/mkdir`）、`embedding()` UDF、`veda_fs()` / `veda_fs_events()` / `veda_storage_stats()` / `search()` 等表函数。支持标准 SELECT/WHERE/COUNT/JOIN。只读 key 调写 UDF 会被拒（当前表现为 `500`）。

### 结构化 Collection（`/v1/collections/*`）

定义 schema + 自动嵌入字段，按字段过滤搜索。`POST /v1/collections`（建）、`GET /v1/collections`（列）、`GET/DELETE /v1/collections/{name}`、`POST /v1/collections/{name}/rows`（插入，body 是 JSON 数组）、`POST /v1/collections/{name}/search`（`{ query, limit? }`）。过滤 / 聚合走 `veda sql`。

### MCP 端点（`POST /mcp`）

给 Coding Agent(Claude Code / Cursor / Codex)的原生工具面——[MCP](https://modelcontextprotocol.io)(Model Context Protocol)Streamable HTTP transport 的 **stateless** 模式,协议版本 `2025-06-18`。用户侧零安装,配置示例见 [AI 助手集成](#/docs/skill)。

- **鉴权**:与 REST 数据面同一道闸——`Authorization: Bearer wk_…`(fs workspace;db kind 返 400)。只读 `wk_` 全功能可用(6 个工具均只读),这是推荐发给消费者的 key。
- **协议行为**:每个 POST 一条 JSON-RPC 消息、回一个 JSON 响应;无 `id` 字段的 notification 返 `202`;不支持 batch;`GET /mcp` 返 `405`(无服务端 SSE 下行流);请求头 `MCP-Protocol-Version` 若存在且非支持版本返 `400`。
- **工具(6 个,均只读)**:`search`(hybrid 检索,`detail_level` 三层)/ `grep`(字面量,带行号,匹配行截断 500B)/ `read_file`(PDF/Word 返提取文本;整读上限 64KB,`start_line`/`end_line` 分页)/ `list_dir`(`recursive` 上限 10000 条)/ `overview`(L1 摘要,未就绪/未启用返回可读提示)/ `ask`(等价 `POST /v1/answer` 非流式,带 citations;与 REST 共享每 workspace 并发上限,超出返回「too many concurrent」提示,10–90s)。
- **错误语义**:协议错误(坏 JSON、未知方法/工具、参数校验)→ JSON-RPC `error`;领域错误(文件不存在、功能未启用、限流、超时)→ `result.isError=true` + 可读文本,调用方 LLM 可据此自愈。
- 冒烟示例:

```bash
curl -s -H "Authorization: Bearer wk_..." -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' https://<入口>/mcp
```

### 变更流（`GET /v1/events`，SSE）

游标式订阅 workspace 变更：`?since_id`（默认 0）、`?path_prefix`。`text/event-stream`，每条事件 `{ id, event_type, path, file_id }`。FUSE / 多实例靠它做近实时失效（~120s 内）。这是**裸 SSE 协议**（错误体不走 `ApiResponse` 信封），`410` 表示游标已过保留窗口、需重新订阅。

---

## 7. 错误码

失败响应固定 `{ "success": false, "error_code": "...", "error": "..." }`。**只匹配 `error_code`**。

| `error_code` | HTTP | 含义 |
|---|---:|---|
| `INVALID_INPUT` | 400 | 通用校验失败（字符集 / 长度 / 缺字段 / 单字段超限）。`error` 带 `<field>: <reason>` |
| `INVALID_PATH` | 400 | 路径非法（fs） |
| `WORKSPACE_KIND_MISMATCH` | 400 | API 打到了错误 kind 的 workspace |
| `CANNOT_DELETE_DEFAULT_DATASET` | 400 | 拒绝删除 `default` dataset |
| `UNAUTHORIZED` | 401 | 缺失 / 无效 / 过期 / 用错面的 token |
| `PERMISSION_DENIED` | 403 | 只读 `wk_` 写、或 `vk_` 操作他账号资源 / 越权 |
| `NOT_FOUND` | 404 | workspace / dataset / key / token / 文件不存在或已归档 |
| `ALREADY_EXISTS` | 409 | 同名冲突（大小写不敏感）/ 邮箱已注册 |
| `PRECONDITION_FAILED` | 412 | CAS 前置条件不满足（`If-Match` revision 不符） |
| `PAYLOAD_TOO_LARGE` | 413 | **仅**批量条数超限（`records`/`ids` >500、`top_k` >100）；单字段超限走 `INVALID_INPUT` |
| `QUOTA_EXCEEDED` | 429 | 保留（当前仅 SQL / fs 列举的扫描上限会触发） |
| `EMBEDDING_FAILED` | 500 | 服务端嵌入上游错误。注意：保留此 code，但 `error` 文案被抹成 `internal server error` |
| `INTERNAL` | 500 | 存储 / 死锁 / 未预期错误的兜底，故意不透出细节 |

---

## 8. 运维端点

| 方法 | 路径 | 鉴权 | 说明 |
|---|---|:--:|---|
| GET | `/healthz` | 🔓 | 存活探针，恒返回 `ok`（纯文本，不查后端）。k8s liveness / systemd watchdog 打这个 |
| GET | `/v1/ready` | 🔓 | 就绪探针，并发 ping MySQL + Milvus（各 3s 超时）；就绪 200 / 否则 503，体含各组件状态 |
| GET | `/capabilities` | 🔓 | 能力位（如 `summary_enabled`），故意不在 `/v1/*` 下 |
| GET | `/install.sh` | 🔓 | CLI 安装脚本（构建期内嵌） |
| GET | `/v1/metrics` | 🛠 token | Prometheus 文本格式。未配 token 或 token 错均返回 **404**（不暴露存在性，无"开放 metrics"模式） |
| POST | `/admin/v1/reconcile/{ws}?dry_run=` | 🛠 token | 按需修复 MySQL↔Milvus 漂移。复用 `metrics_token`，默认 `dry_run=true` 只报告；失败响亮返 500 |

`metrics_token`（也门控 reconcile）通过 `VEDA_METRICS_TOKEN` 或 TOML 配置，常量时间比较。

### 关键配置（operator）

`[mysql] [milvus] [embedding]` 必填，`[llm] [otlp]` 可选。常用环境变量覆盖：

| 环境变量 | 默认 | 说明 |
|---|---|---|
| `VEDA_LISTEN` | `0.0.0.0:3000` | 监听地址 |
| `VEDA_MYSQL_URL` | — | MySQL 连接串（必填） |
| `VEDA_MILVUS_URL` | — | Milvus 地址（必填） |
| `VEDA_EMBEDDING_API_URL` / `_API_KEY` / `_MODEL` / `_DIMENSION` | — | 嵌入服务（必填） |
| `VEDA_EMBEDDING_BATCH_SIZE` | 100 | 阿里百炼 / DashScope 要设 `10`（其 `input.contents` 上限 10） |
| `VEDA_LLM_API_URL` | — | 配上才启用摘要功能（不配则 `/v1/abstract` 等返 501） |
| `VEDA_ALLOWED_ORIGINS` | `[]` | CORS 白名单（逗号分隔）；生产必须显式列域名 |
| `VEDA_METRICS_TOKEN` | 无 | 门控 `/v1/metrics` 和 reconcile |
| `VEDA_OTLP_ENABLED` | false | OTLP（metric/trace 发本地 agent） |

---

## 9. 已知限制与边界

诚实说清楚现在不擅长什么、上线前要盯什么：

**能力边界**
- 只支持 **UTF-8 文本**；PDF / 图片 / 视频不解析（`extract_text` 是占位，OCR 未做），二进制在客户端就被拒。
- 隔离只到 **workspace 级**：持某 workspace `wk_` 的人能看到它全部内容，无行级 / 文档级 ACL、无字段级权限。HR / 合规这类多人混合敏感场景目前不适配。
- 不是 OLTP 数据库，是知识库；高并发交易场景不合适。

**规模与吞吐（上线前重点）**
- **嵌入吞吐受云商 QPM 硬限**：当前无客户端并发闸，压测下 hybrid 检索会塌（~36 QPS / p99 ~4s / 25% 429）。大批量导入请控制并发、用 `fulltext` mode（不走嵌入）或分批退避。
- **db workspace 数量天花板**：每个 db workspace 一个常驻 Milvus collection（建库即 load、不 unload），库数量受 Milvus 内存上限约束，超限会导致新建库失败。海量库需等懒加载 / 多副本演进。
- **写入吞吐 << 读取**：Milvus 写入在中等并发就排队。批量写优先用 `write_mode=insert`（id 天然唯一时）+ 合理批大小（≤500/次）。
- **单进程单副本 alpha**：server 与 worker 同进程，无 HA、无 Docker/Helm；这是当前部署形态的可用性上限。

**数据语义需注意**
- 软删 workspace / dataset **不回收 Milvus 向量**（存储泄漏，删除后名称暂不可复用）。
- `write_mode=insert` 重复 pk 是 **Milvus 未定义行为**（物理堆多行、compaction 不自动清）；怕重试 / 会重导的管道必须用默认 `upsert`。
- 向量 `delete` 的 `delete_count` 是 tombstone 数 = `len(ids)`，**不代表真实删除行数**。
- 无 OpenAPI；Java SDK 仍是旧 `vk_` 契约（**未适配 `wk_`，拿旧 SDK 打新 server 会鉴权失败**）。

---

## 10. SDK 与示例

- **Java SDK**：`sdk/java`（Java 8 + Jackson + OkHttp），封装向量库数据面 4 端点。⚠️ 仍待适配 2026-06 的 `wk_` 契约。
- **Python 示例**：`examples/python_pinecone_demo.py`（无 SDK，裸 HTTP）。
- **CLI**：`curl -fsSL https://veda.ddmc-inc.com/install.sh | sh`，详见 [CLI 速查](#/docs/cli)。

发现问题去 [git.ddxq.mobi/middleware/dbpaas/veda](http://git.ddxq.mobi/middleware/dbpaas/veda) 提 issue。
