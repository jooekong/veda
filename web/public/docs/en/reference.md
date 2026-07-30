# Reference

This is the complete technical reference for Veda: architecture, authentication, the two data planes, error codes, ops endpoints, and the boundaries you should know about. For a fast end-to-end run, see [Quickstart](#/docs/quickstart); if you only care about the vector workspace, see [Vector Workspace API](#/docs/vectors); this page puts everything in one place — the one to keep open as a manual.

---

## 1. What is Veda

One sentence: **a knowledge store that puts files, vector search, and SQL queries behind a single API**. The backend is MySQL (control plane: metadata, accounts, job queue) + Milvus (data plane: vectors, full-text, structured data) + an async worker (embeddings, summaries).

There are two workspace kinds, chosen at creation and immutable afterwards; the kind decides which data plane it runs on:

| | File Workspace | Vector Workspace |
|---|---|---|
| `kind` | `fs` (default) | `db` |
| Data model | files / directories | vector records (text + meta) |
| Data plane | `/v1/fs/*`, `/v1/search`, `/v1/grep`, `/v1/sql`, `/v1/abstract`, `/v1/overview`, `/v1/collections/*`, `/v1/events`, FUSE | `/v1/vectors/{upsert,search,query,delete}` |
| Access | CLI / FUSE / HTTP | REST API / SDK |
| Typical use | personal knowledge base, agent memory, code search | managed vector retrieval for applications |

The two lines **never cross**: a file workspace's `wk_` can't call vector endpoints, and vice versa (blocked with `400 WORKSPACE_KIND_MISMATCH`). They are two independent product lines sharing one account system, one auth model, and one ops surface.

**Mental model**: one **account** owns multiple **workspaces**; each workspace is an isolation boundary. The control plane (create workspaces, issue keys, manage datasets) uses the account-level `vk_`; the data plane (read/write data) uses the workspace-level `wk_`.

---

## 2. Authentication model (read this first)

All credentials go in `Authorization: Bearer <token>`. Two key types, two planes — don't mix them:

| Key | Prefix | Plane | Bound to | Purpose |
|---|---|---|---|---|
| **Account key** | `vk_` | control plane | the whole account | create/delete workspaces, manage datasets, issue `wk_`, mint service tokens |
| **Workspace key** | `wk_` | data plane | a single workspace | read/write that workspace's data (same for fs and db) |

Rules:

- **A `wk_` is bound to a single workspace.** Data-plane request bodies therefore **carry no `workspace_id`** — the target is decided by the key. fs uses the internal `AuthWorkspace`, db uses `AuthDbWorkspace`; each validates `kind`, and a mismatch is `400 WORKSPACE_KIND_MISMATCH`.
- **`wk_` comes in two permissions**: `read` / `readwrite`. A read-only key can search / query / read files, but cannot upsert / delete / write files (→ `403 PERMISSION_DENIED`).
- **`vk_` lives on the control plane only**, held by the platform / console — **never handed out to application teams**. An application team normally holds exactly one `wk_`.
- **Wrong plane = 401**: a `vk_` hitting the data plane, or a `wk_` hitting the control plane, is rejected as an invalid credential with `401 UNAUTHORIZED`.
- **Revocation is immediate**: archiving a workspace sets all of its `wk_` to `revoked` in the same transaction; suspending an account invalidates all of its keys (`vk_` + `wk_`) on the very next request.
- **JWT is fully removed**: no `POST /v1/workspaces/{id}/token`, no `jwt_secret` — auth is pure key validation (keys are stored as SHA-256 hashes).

> A `vk_` is account-root authority — there are no capability tiers. To restrict a token to a few specific workspaces, use the `allowed_workspaces` scope on `POST /admin/v1/tokens` (see §4.5).

### Platform integration (AI Platform)

The company AI Platform uses a separate `app_id`-keyed control plane at `/v1/apps/{app_id}/*`: authentication is pushed out to the platform gateway, veda trusts the `app_id` in the path, and accounts are auto-provisioned on first access. Application teams still only get a `wk_` under this model. See §4.6.

---

## 3. Global conventions

### Base URL

`<BASE>` depends on the deployment. Company deployment example: `https://veda.ddmc-inc.com`. The server listens on `0.0.0.0:3000` by default. Examples below use `$BASE` throughout. Business endpoints all live under `/v1/*` or `/admin/v1/*`.

### Response envelope

With few exceptions, every endpoint that returns a body uses `ApiResponse<T>`:

```json
// success
{ "success": true, "data": { /* T, endpoint-specific */ } }
// failure
{ "success": false, "error_code": "INVALID_INPUT", "error": "text: must not be empty" }
```

- `error_code` is a **stable machine-readable code**, present only on failure. **Clients must match on `error_code` only — never parse the `error` message** (the wording can change at any time, and some internal errors have their message scrubbed to `internal server error`).
- Success responses carry no `error_code` / `error`; failure responses carry no `data`.
- **Exceptions**: some delete endpoints return `204 No Content` (no envelope); `/v1/events` is a raw SSE stream; ops endpoints (`/healthz`, `/v1/ready`, `/v1/metrics`) each have their own format.

### HTTP status codes

Success uses `200` (general) / `201` (create dataset, mint token, create workspace on the apps plane) / `204` (delete dataset, revoke key, disable token). On failure, status codes map one-to-one to `error_code` — see §7.

> ⚠️ One status-code asymmetry: creating a workspace via direct `vk_` (`POST /v1/workspaces`) returns **200**, but via the apps platform plane (`POST /v1/apps/{app_id}/workspaces`) returns **201**. SDKs that target both planes must special-case this.

### Time formats (two coexist)

| Where it appears | Type | Example |
|---|---|---|
| Control-plane objects (`created_at` / `updated_at` on Workspace / Dataset / Key) | **RFC3339 string** | `"2026-05-29T12:34:56Z"` |
| `created_at` / `updated_at` on vector hits, `commit_ts` on `upsert`, `expires_at` in the mint-token request | **int64 epoch milliseconds** | `1735689600000` |

Don't unify the two when generating types.

### Pagination

`GET /v1/workspaces` and `GET /v1/workspaces/{ws}/datasets` use cursor pagination:

```json
{ "success": true, "data": {
  "items": [ /* ... */ ], "has_more": true, "next_cursor": "<pass as after next time>"
} }
```

- `limit`: page size, default 100, max 200 (silently clamped beyond that).
- `after`: the previous page's `next_cursor`, an opaque string. When `has_more=false`, `next_cursor` is absent.
- Ordering is stable but implementation-defined (ascending row id, lexicographic for UUIDs) — **not a business-meaningful order**. If you need a specific order, fetch everything and sort client-side.
- Note: `GET /v1/workspaces/{id}/keys` is **not paginated** — it returns a plain array.

---

## 4. Control plane API (🔑 `vk_`)

Summary (🔑 `vk_` account auth · 🔓 no auth · 🏢 apps platform plane, no veda credential · 🛠 ops `metrics_token`):

| Method | Path | Auth | Success | Purpose |
|---|---|:--:|:--:|---|
| POST | `/v1/accounts` | 🔓 | 200 | Register an account (email mode / app_id mode) |
| POST | `/v1/accounts/anonymous` | 🔓 | 200 | Anonymous onboarding (creates an **fs** workspace) |
| POST | `/v1/accounts/claim` | 🔑 | 200 | Claim an anonymous account |
| POST | `/v1/accounts/login` | 🔓 | 200 | Email login for a fresh `vk_` |
| POST | `/v1/workspaces` | 🔑 | 200 | Create a workspace |
| GET | `/v1/workspaces` | 🔑 | 200 | List workspaces (paginated) |
| DELETE | `/v1/workspaces/{id}` | 🔑 | 200 | Soft-delete a workspace |
| POST | `/v1/workspaces/{id}/keys` | 🔑 | 200 | Issue a data-plane `wk_` |
| GET | `/v1/workspaces/{id}/keys` | 🔑 | 200 | List `wk_` metadata (not paginated) |
| DELETE | `/v1/workspaces/{id}/keys/{key_id}` | 🔑 | 204 | Revoke a `wk_` |
| POST | `/v1/workspaces/{ws}/datasets` | 🔑 | 201 | Create a dataset |
| GET | `/v1/workspaces/{ws}/datasets` | 🔑 | 200 | List datasets (paginated) |
| DELETE | `/v1/workspaces/{ws}/datasets/{name}` | 🔑 | 204 | Soft-delete a dataset (`default` is protected) |
| POST | `/admin/v1/tokens` | 🔑 | 201 | Mint an account-level scoped `vk_` service token |
| POST | `/admin/v1/tokens/{id}/disable` | 🔑 | 204 | Revoke a token |
| POST | `/v1/apps/{app_id}/workspaces` | 🏢 | 201 | Create a workspace (platform plane) |
| GET | `/v1/apps/{app_id}/workspaces` | 🏢 | 200 | List workspaces (platform plane) |
| DELETE | `/v1/apps/{app_id}/workspaces/{id}` | 🏢 | 200 | Delete a workspace (platform plane) |

### 4.1 Accounts

**POST `/v1/accounts`** 🔓 — two mutually exclusive modes:
- **email mode** (console / CLI): `{ name, email, password }`, returns `{ account_id, api_key: "vk_…" }`. Email already registered → `409 ALREADY_EXISTS`.
- **app_id mode** (platform): `{ name, app_id }` (**no email/password**), returns `{ account_id, api_key: "vk_…", app_id }`. `app_id` is unique; conflict → `409`.
- Mixed input (app_id + email/password) or neither → `400 INVALID_INPUT`.

> ⚠️ In v0 this endpoint is **public** — anyone can squat any `app_id`. Trusted internal networks only; a platform credential is mandatory before going public.

**POST `/v1/accounts/anonymous`** 🔓 — zero input; returns `{ account_id, api_key: "vk_…", workspace_id, workspace_key: "wk_…" }` in one shot. **Note: the workspace it creates is `kind=fs`** — it cannot be used as a vector workspace.

**POST `/v1/accounts/claim`** 🔑 — upgrades an anonymous account into a named one (adds email + password); the original `vk_` stays valid. Already claimed / app_id account / empty fields → `400`; email taken → `409`.

**POST `/v1/accounts/login`** 🔓 — email + password login; mints a fresh `vk_` (name=`login`) and **revokes all of the account's previous `login` keys**. Unknown account / wrong password / suspended all return a uniform `401` (no existence leak).

### 4.2 Workspaces

**POST `/v1/workspaces`** 🔑 — `{ name, kind?: "fs"|"db", description? }` (`kind` defaults to `fs` when omitted; **a vector workspace requires an explicit `"db"`**). For `kind=db`, the server creates the workspace + `default` dataset in a single transaction, then provisions the Milvus collection (rolled back on failure — no zombies left behind). Returns the full `Workspace` object (200). Duplicate name → `409`; Milvus provisioning failure → `500` (already rolled back).

**GET `/v1/workspaces`** 🔑 — lists all workspaces on the account (fs + db), paginated.

**DELETE `/v1/workspaces/{id}`** 🔑 — soft delete (`status=archived`); revokes all of its `wk_` in the same transaction. Returns **200** `{ "success": true, "data": null }`. Not your account → `403`; not found → `404`.
> ⚠️ Currently a soft delete: **Milvus vectors are not reclaimed** (storage leak), and the name cannot be reused for now.

### 4.3 Workspace keys (lifecycle of the data-plane `wk_`)

**POST `/v1/workspaces/{id}/keys`** 🔑 — `{ name?, permission?: "read"|"readwrite" }` (`permission` defaults to `readwrite`). Returns `{ "key": "wk_…", "permission": "…" }` — **the plaintext appears exactly once**. Archived workspaces can no longer issue keys (→ `404`).

**GET `/v1/workspaces/{id}/keys`** 🔑 — returns `WorkspaceKey[]` (**plain array, not paginated**, metadata only — no plaintext). Each item carries `id / workspace_id / account_id / name / permission / status / kind / created_at`.

**DELETE `/v1/workspaces/{id}/keys/{key_id}`** 🔑 — revoke (`status=revoked`, effective immediately). Returns **204**. Key not under this workspace → `404`.

### 4.4 Datasets (db workspaces only)

All three endpoints require the target workspace to be `kind=db` and active — otherwise `400 WORKSPACE_KIND_MISMATCH` / `404`.

**POST `/v1/workspaces/{ws}/datasets`** 🔑 — `{ name, description? }` (`name` charset `[a-zA-Z0-9_-]+`, ≤64 bytes, no `:`). Returns **201** `Dataset`. Duplicate name (case-insensitive) → `409`.

**GET `/v1/workspaces/{ws}/datasets`** 🔑 — lists active datasets, paginated.

**DELETE `/v1/workspaces/{ws}/datasets/{name}`** 🔑 — soft delete (`status=archived`), returns **204**. `default` cannot be deleted (case-insensitive) → `400 CANNOT_DELETE_DEFAULT_DATASET`; not found → `404`. Milvus vectors are likewise not reclaimed.

### 4.5 Service tokens (scoped `vk_`)

**POST `/admin/v1/tokens`** 🔑 — mints an account-level scoped `vk_` owned by the caller's account. v0 has no separate admin gateway: any holder of an account `vk_` can mint tokens for **their own** account. **This is a control-plane `vk_`, not a data-plane `wk_`.**

```json
{ "app_id": "search-svc", "name": "prod",
  "allowed_workspaces": ["<ws_id>"], "expires_at": 1767225600000 }
```

`allowed_workspaces`? Omitted = unrestricted within the account; every listed workspace is checked for account ownership (foreign workspace → `403`). `expires_at`? Epoch milliseconds; omitted = never expires. Returns **201** `{ id, token: "vk_…" }` — the `token` appears exactly once.

**POST `/admin/v1/tokens/{id}/disable`** 🔑 — revoke (ownership checked first). Returns **204**. Not found or owned by another account → `404` either way (no existence leak).

> On a token, `app_id` is just a **governance label**, not a security boundary; the real isolation is `allowed_workspaces` + `workspace.kind`.

### 4.6 The apps platform plane (🏢 `/v1/apps/{app_id}/*`)

For the company AI Platform. These endpoints **don't read `Authorization`** — the platform gateway has already proven the caller may act for the `app_id`, so veda trusts the `app_id` in the path to resolve the account / auto-provision it. Coexists with the direct `vk_` plane above.

- **POST `/v1/apps/{app_id}/workspaces`** — on first access for an `app_id`, the account is **auto-provisioned** (passwordless, no `vk_` minted), then the workspace is created (any `app_id` in the body is overridden by the path). Returns **201** `Workspace`.
- **GET `/v1/apps/{app_id}/workspaces`** — lists the app's workspaces. Unknown app_id → **empty page** (no auto-provisioning; GET has no side effects). Returns 200, paginated.
- **DELETE `/v1/apps/{app_id}/workspaces/{id}`** — soft delete. Cross-tenant and not-found both return `404`. Returns **200**.
- Suspended account → all of these endpoints return `401` (`account suspended`).

app_id accounts are passwordless: no login, no claim — `app_id` and `(email, password)` are mutually exclusive on one account.

---

## 5. Vector workspace data plane (🟦 `wk_`)

Four endpoints; the target workspace is bound by the `wk_`, and request bodies **carry no `workspace_id`**. The only common parameter is `dataset`? (falls back to `default` when omitted). `upsert` / `delete` require a `readwrite` `wk_` (read-only → `403`).

| Method | Path | Permission | Purpose |
|---|---|---|---|
| POST | `/v1/vectors/upsert` | readwrite | Write / overwrite records, ≤500 per call |
| POST | `/v1/vectors/search` | read | Vector search (three modes) |
| POST | `/v1/vectors/query` | read | Direct lookup by id, ≤500 per call |
| POST | `/v1/vectors/delete` | readwrite | Delete by id, ≤500 per call |

**Core concepts**:
- **Dataset**: a logical grouping inside a workspace (e.g. `products`, `faq`); all datasets share one collection, separated by the scalar field `dataset`. Every db workspace comes with a `default` dataset (undeletable; the fallback when `dataset` is omitted).
- **Record**: one row of data. `text` is required (the server embeds it and builds the BM25 index from it); `id` / `category` / `tags` / `meta` are optional. The physical primary key `{dataset}:{id}` is assembled server-side and never appears on the wire.

**Write semantics (`write_mode`)**: optional top-level `write_mode`: `"upsert"` (default, idempotent and safe) / `"insert"` (skips the duplicate check, ~3x faster, **the caller must guarantee pk uniqueness**). Records without an `id` always take the insert fast path (UUIDs can't collide).

**Search modes (`mode`)**:

| `mode` | Behavior | Embeds the query? | `score_type` | Score range |
|---|---|:--:|---|---|
| `hybrid` (default) | dense ANN + BM25, fused with RRF | yes | `rrf` | ~[0, 0.033] |
| `semantic` | dense ANN over the embedded query | yes | `cosine` | ~[0, 1] |
| `fulltext` | BM25 over the tokenized `text` | no | `bm25` | ~[0, 30+] |

Scores are **not comparable across modes** — check `score_type` before reading a score. `hybrid` fails loudly; it **never silently degrades**. `min_score` (relevance floor) only applies to `semantic`/`fulltext`; combining it with `hybrid` is a `400`.

> For the vector data plane's **complete fields, request/response examples, filter DSL, limits, idempotency, and error codes**, see [Vector Workspace API](#/docs/vectors). This section is the product-level overview; that page is the field-by-field contract.

---

## 6. File workspace data plane (🟦 `wk_`, `kind=fs`)

All fs endpoints authenticate with a `wk_` (bound to an fs workspace); writes require `readwrite`. For the full CLI see [CLI reference](#/docs/cli); for the local-directory form see [FUSE mount](#/docs/fuse).

### File CRUD (`/v1/fs/*`, 50MB upload body limit)

| Method / Path | Description |
|---|---|
| `PUT /v1/fs/{path}` | Write raw bytes. Valid UTF-8 is stored, chunked, and indexed as text; non-UTF-8 is stored verbatim as a blob (PDFs additionally have their text extracted for search; other binaries such as images are not indexed). Supports `If-Match: "<rev>"` (CAS; mismatch → `412`) and `If-None-Match: "<sha256>"` (skips the rewrite when content is identical, returning `content_unchanged:true`). Returns `{ file_id, revision, content_unchanged }` + `ETag`. A directory path → `409`; maximum file size 50MB. |
| `POST /v1/fs/{path}` | Append content (no CAS). |
| `GET /v1/fs/{path}` | Read raw bytes with the stored `Content-Type`. `?stat` for metadata, `?list` for directory listing, `?lines=start:end` for a line slice; `Range: bytes=a-b` returns `206`. No parameters returns the full content. |
| `HEAD /v1/fs/{path}` | Fetch `FileInfo` (path/file_id/is_dir/size/mime/revision/checksum/timestamps). |
| `DELETE /v1/fs/{path}` | Delete (directories recurse). |
| `POST /v1/fs-copy` | `{ from, to }` server-side copy (content-addressed dedup). |
| `POST /v1/fs-rename` | `{ from, to }` rename. |
| `POST /v1/fs-mkdir` | `{ path }` create a directory. |
| `POST /v1/grep` | `{ pattern, path_prefix?, ignore_case?, max_results?=100 }` literal substring scan (not regex, synchronous); returns `{path, line_no, line}[]`. |

> Deleting the root — `DELETE /v1/fs` — always returns `400` (forbidden).

**Indexing progress**: `GET /v1/index-status` returns this workspace's backlog of index-gating tasks as `{pending, processing, dead}` (chunk/extract tasks only — the ones that gate searchability). Poll it after batch uploads to answer "is everything searchable yet"; `dead > 0` means some files permanently failed to index and need an operator. CLI: `veda status --index [--wait]` (`--wait` polls until drained; exits non-zero on dead > 0 — usable as a CI gate).

### Search (`POST /v1/search`)

`{ query, mode?, limit?, path_prefix?, detail_level? }`. `mode` defaults to `hybrid` (same three modes as the vector workspace); `limit` defaults to 10, max 100; `detail_level` defaults to `full`. Returns `SearchHit[]`; every hit carries a `score_type` (`rrf`/`bm25`/`cosine` — not comparable across types). Embedding is **asynchronous**: a freshly written file takes a few seconds to become searchable.

### Three-tier information model (L0 / L1 / L2)

Every file / directory gets auto-generated layered summaries — fetch on demand, save tokens:

| Tier | Endpoint | Size | Meaning |
|---|---|---|---|
| **L0 Abstract** | `GET /v1/abstract/{path}` or search `detail_level=abstract` | one sentence | one-sentence summary (files and directories both go into Milvus and can be hit by vector search) |
| **L1 Overview** | `GET /v1/overview/{path}` or `detail_level=overview` | ~2k tokens | structured overview |
| **L2 Full** | `GET /v1/fs/{path}` or search `detail_level=full` (default) | full text | raw content chunks |

`/v1/abstract` and `/v1/overview` are **tri-state**: `200` (ready) / `202 + Retry-After:5` (generating) / `501 + Cache-Control:no-store` (server has no `[llm]` configured; summaries disabled). Summaries depend on the optional LLM config and are automatically disabled without it. The root `/` has no summary (no root dentry).

### RAG answering (`POST /v1/answer`)

One call, one answer **with verifiable citations**: a server-side LLM loop retrieves on its own (search + read_file tool rounds) and answers with inline `[n]` markers. Requires `[llm]` configured server-side; fs workspaces only.

- **Request**: `{ query (≤1024 chars), path_prefix?, limit? (pre-search count, default 12, cap 24), prompt? (custom bot persona, ≤4000) }`.
- **Response**: `{ answer, citations: [{index, path, spans}], hit_count, estimated_context_tokens }`. `spans` are chunk ranges; **an empty array means the whole file**. Two passages of one file yield two citations with the same path (chunk granularity, by design) — display layers should group by path. When nothing supports an answer, a fixed refusal phrase comes back with empty citations (no fabrication).
- **Streaming**: `POST /v1/answer/stream` (SSE), five events: `delta` / `reset` (drop accumulated deltas) / `tool` (progress note) / `final` (authoritative full result — consumers must replace accumulated text with it) / `error`.
- **Errors**: `429 THROTTLED` (per-workspace concurrency cap, default 2); `501 FEATURE_DISABLED` (no LLM configured); `504 ANSWER_TIMEOUT` (90s deadline). Typical latency 10–90s.
- **CLI**: `veda ask "question" [--path PREFIX] [--json]`; the MCP `ask` tool exposes the same capability.

### SQL (`POST /v1/sql`)

`{ sql }`, DataFusion engine, scoped to the workspace. Tables: `files` (recursive dentries), plus every structured collection registered as a table by name. Built-ins: 8 FS scalar UDFs (`veda_read/write/append/exists/size/mtime/remove/mkdir`), the `embedding()` UDF, and table functions like `veda_fs()` / `veda_fs_events()` / `veda_storage_stats()` / `search()`. Standard SELECT/WHERE/COUNT/JOIN supported. A read-only key calling a write UDF is rejected (currently surfaces as a `500`).

### Structured collections (`/v1/collections/*`)

Define a schema + an auto-embedded field, then filter and search by field. `POST /v1/collections` (create), `GET /v1/collections` (list), `GET/DELETE /v1/collections/{name}`, `POST /v1/collections/{name}/rows` (insert; body is a JSON array), `POST /v1/collections/{name}/search` (`{ query, limit? }`). Filters / aggregates go through `veda sql`.

### MCP endpoint (`POST /mcp`)

Native tool surface for coding agents (Claude Code / Cursor / Codex) — [MCP](https://modelcontextprotocol.io) Streamable HTTP transport in **stateless** mode, protocol revision `2025-06-18`. Zero client install; config examples in [AI agent integration](#/docs/skill).

- **Auth**: the same gate as the REST data plane — `Authorization: Bearer wk_…` (fs workspace; db kind → 400). A read-only `wk_` runs every tool (all six are read-only) and is the recommended key to hand to consumers.
- **Protocol behavior**: one JSON-RPC message per POST, one JSON response; notifications (no `id` member) → `202`; batches unsupported; `GET /mcp` → `405` (no server-initiated SSE stream); an `MCP-Protocol-Version` header with an unsupported value → `400`.
- **Tools (6, all read-only)**: `search` (hybrid, tiered `detail_level`) / `grep` (literal, line numbers, matched lines clipped at 500B) / `read_file` (PDF/Word return extracted text; whole-file reads capped at 64KB, page with `start_line`/`end_line`) / `list_dir` (`recursive` capped at 10000 entries) / `overview` (L1 summary; pending/disabled return readable notices) / `ask` (equivalent to non-streaming `POST /v1/answer`, with citations; shares the per-workspace concurrency cap with REST — excess returns a "too many concurrent" notice; 10–90s).
- **Error split**: protocol errors (bad JSON, unknown method/tool, invalid params) → JSON-RPC `error`; domain errors (missing file, feature disabled, throttled, timeout) → `result.isError=true` with readable text the calling LLM can react to.
- Smoke test:

```bash
curl -s -H "Authorization: Bearer wk_..." -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' https://veda.ddmc-inc.com/mcp
```

### Change stream (`GET /v1/events`, SSE)

Cursor-based subscription to workspace changes: `?since_id` (default 0), `?path_prefix`. `text/event-stream`; each event is `{ id, event_type, path, file_id }`. FUSE / multi-instance setups rely on it for near-real-time invalidation (within ~120s). This is **raw SSE** (error bodies don't use the `ApiResponse` envelope); `410` means the cursor has fallen out of the retention window — resubscribe.

---

## 7. Error codes

Failure responses are always `{ "success": false, "error_code": "...", "error": "..." }`. **Match on `error_code` only.**

| `error_code` | HTTP | Meaning |
|---|---:|---|
| `INVALID_INPUT` | 400 | Generic validation failure (charset / length / missing field / single field over limit). `error` carries `<field>: <reason>` |
| `INVALID_PATH` | 400 | Invalid path (fs) |
| `WORKSPACE_KIND_MISMATCH` | 400 | API hit a workspace of the wrong kind |
| `CANNOT_DELETE_DEFAULT_DATASET` | 400 | Refused to delete the `default` dataset |
| `UNAUTHORIZED` | 401 | Missing / invalid / expired / wrong-plane token |
| `PERMISSION_DENIED` | 403 | Read-only `wk_` writing, or a `vk_` touching another account's resources / exceeding scope |
| `NOT_FOUND` | 404 | Workspace / dataset / key / token / file missing or archived |
| `ALREADY_EXISTS` | 409 | Duplicate name (case-insensitive) / email already registered |
| `PRECONDITION_FAILED` | 412 | CAS precondition not met (`If-Match` revision mismatch) |
| `PAYLOAD_TOO_LARGE` | 413 | **Only** batch-count overruns (`records`/`ids` >500, `top_k` >100); single-field overruns are `INVALID_INPUT` |
| `QUOTA_EXCEEDED` | 429 | Reserved (currently only triggered by SQL / fs listing scan caps) |
| `EMBEDDING_FAILED` | 500 | Upstream embedding error on the server. Note: the code is kept, but the `error` message is scrubbed to `internal server error` |
| `INTERNAL` | 500 | Catch-all for storage / deadlock / unexpected errors; details deliberately withheld |

---

## 8. Ops endpoints

| Method | Path | Auth | Description |
|---|---|:--:|---|
| GET | `/healthz` | 🔓 | Liveness probe; always returns `ok` (plain text, no backend checks). Point k8s liveness / systemd watchdog here |
| GET | `/v1/ready` | 🔓 | Readiness probe; pings MySQL + Milvus concurrently (3s timeout each); 200 when ready / 503 otherwise, body includes per-component status |
| GET | `/capabilities` | 🔓 | Capability flags (e.g. `summary_enabled`); deliberately outside `/v1/*` |
| GET | `/install.sh` | 🔓 | CLI install script (embedded at build time) |
| GET | `/v1/metrics` | 🛠 token | Prometheus text format. Token unconfigured or wrong both return **404** (no existence exposure, no "open metrics" mode) |
| POST | `/admin/v1/reconcile/{ws}?dry_run=` | 🛠 token | On-demand repair of MySQL↔Milvus drift. Reuses the `metrics_token`; default `dry_run=true` only reports; failures return a loud 500 |

The `metrics_token` (also gating reconcile) is set via `VEDA_METRICS_TOKEN` or TOML config, compared in constant time.

### Key configuration (operator)

`[mysql]` `[milvus]` `[embedding]` are required; `[llm]` `[otlp]` are optional. Common environment variable overrides:

| Environment variable | Default | Description |
|---|---|---|
| `VEDA_LISTEN` | `0.0.0.0:3000` | Listen address |
| `VEDA_MYSQL_URL` | — | MySQL connection string (required) |
| `VEDA_MILVUS_URL` | — | Milvus address (required) |
| `VEDA_EMBEDDING_API_URL` / `_API_KEY` / `_MODEL` / `_DIMENSION` | — | Embedding service (required) |
| `VEDA_EMBEDDING_BATCH_SIZE` | 100 | Set to `10` for Alibaba Bailian / DashScope (its `input.contents` cap is 10) |
| `VEDA_EMBEDDING_MAX_CONCURRENCY` | 8 | Upstream embedding concurrency gate (interactive retrieval takes permits ahead of background indexing); governs search latency during bulk imports and 429 exposure |
| `VEDA_LLM_API_URL` | — | Enables summaries when set (otherwise `/v1/abstract` etc. return 501) |
| `VEDA_ALLOWED_ORIGINS` | `[]` | CORS allowlist (comma-separated); production must list domains explicitly |
| `VEDA_METRICS_TOKEN` | unset | Gates `/v1/metrics` and reconcile |
| `VEDA_OTLP_ENABLED` | false | OTLP (metrics/traces to a local agent) |

---

## 9. Known limits and boundaries

An honest list of what it's not good at today and what to watch before going to production:

**Capability boundaries**
- Images / video / scanned PDFs (no text layer) are not parsed — **no OCR yet**. PDF / Word files get their text extracted and indexed automatically; other binaries (non-UTF-8) are stored verbatim as blobs — downloadable byte-for-byte, but not indexed.
- Isolation stops at the **workspace level**: anyone holding a workspace's `wk_` sees all of its content — no row-level / document-level ACLs, no field-level permissions. Mixed-sensitivity multi-user scenarios like HR / compliance are not a fit today.
- It's a knowledge store, not an OLTP database; high-concurrency transactional workloads don't belong here.

**Scale and throughput (the pre-production checklist)**
- **Embedding throughput is hard-capped by the cloud provider's QPM**: there is no client-side concurrency gate yet, and hybrid search collapses under load tests (~36 QPS / p99 ~4s / 25% 429). For bulk imports, throttle concurrency, use `fulltext` mode (no embedding), or batch with backoff.
- **db workspace count has a ceiling**: each db workspace is one resident Milvus collection (loaded at creation, never unloaded), so the count is bounded by Milvus memory; past the limit, new workspace creation fails. Massive workspace counts have to wait for the lazy-loading / multi-replica evolution.
- **Write throughput << read**: Milvus writes start queueing at moderate concurrency. For bulk writes, prefer `write_mode=insert` (when ids are naturally unique) + sensible batch sizes (≤500 per call).
- **Single-process, single-replica alpha**: server and worker share one process; no HA, no Docker/Helm. That's the availability ceiling of the current deployment shape.

**Data semantics to watch**
- Soft-deleting a workspace / dataset **does not reclaim Milvus vectors** (storage leak; the name cannot be reused after deletion for now).
- Duplicate pks under `write_mode=insert` are **undefined behavior in Milvus** (multiple physical rows pile up; compaction doesn't clean them automatically). Pipelines that retry or re-import must use the default `upsert`.
- The vector `delete`'s `delete_count` is the tombstone count = `len(ids)` — **not the number of rows actually deleted**.
- No OpenAPI spec; the Java SDK still speaks the old `vk_` contract (**not adapted to `wk_` — the old SDK against the new server fails auth**).

---

## 10. SDKs and examples

- **Java SDK**: `sdk/java` (Java 8 + Jackson + OkHttp), wraps the 4 vector data-plane endpoints. ⚠️ Still pending adaptation to the 2026-06 `wk_` contract.
- **Python example**: `examples/python_pinecone_demo.py` (no SDK, raw HTTP).
- **CLI**: `curl -fsSL https://veda.ddmc-inc.com/install.sh | sh` — see [CLI reference](#/docs/cli).

Found a problem? File an issue at [git.ddxq.mobi/middleware/dbpaas/veda](http://git.ddxq.mobi/middleware/dbpaas/veda).
