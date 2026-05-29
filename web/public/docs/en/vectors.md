# Vector Workspace API

A **Vector Workspace** is a Veda workspace of `kind=db`: a Pinecone-style managed vector service — write text → the server embeds it → retrieve by semantic similarity, with meta filtering. Built for business apps integrating over REST API / SDK.

> As opposed to a **File Workspace** (`kind=fs`, file storage + CLI/FUSE). The two have different data models and access paths and don't interoperate — a File Workspace's `wk_` / JWT / FUSE do **not** apply to Vector Workspaces.

---

## 1. Conventions

- HTTP + JSON (UTF-8), `Content-Type: application/json`.
- All endpoints live under `/v1/*` or `/admin/v1/*`. `<BASE_URL>` is deployment-defined; examples below use `$BASE`.

**Response envelope** — success:
```json
{ "success": true, "data": { /* per endpoint */ } }
```
failure:
```json
{ "success": false, "error_code": "INVALID_INPUT", "error": "text: must not be empty" }
```
- `error_code` is a **stable machine-readable code**, present only on failure. **Match on `error_code`, never parse the `error` text.**
- Some delete endpoints return `204 No Content` with no body (see below).

**Two time formats — distinguish them when deserializing:**

| Where | Type | Example |
|---|---|---|
| Control-plane objects (Workspace / Dataset `created_at` / `updated_at`) | RFC3339 string | `"2026-05-29T12:34:56Z"` |
| Vector hit `created_at` / `updated_at`, `upsert` `commit_ts`, token-mint `expires_at` | int64 ms epoch | `1735689600000` |

---

## 2. Authentication

Credentials go in `Authorization: Bearer <token>`. **Vector Workspaces accept only account-level `vk_`:**

| Credential | Prefix | For Vector Workspaces? |
|---|---|:--:|
| Account key / service token | `vk_` | ✅ yes |
| Workspace key | `wk_` | ❌ no (File Workspace only) |
| Workspace JWT | none | ❌ no (File Workspace only) |

Using `wk_` or a JWT on a Vector Workspace endpoint returns `401 UNAUTHORIZED`.

**Scope**: a service token (`POST /admin/v1/tokens`) may carry `allowed_workspaces` to restrict which workspaces it can touch. If a vector endpoint **omits** `workspace_id`, it is inferred only when the token's `allowed_workspaces` has exactly one entry; otherwise it must be passed explicitly.

---

## 3. Quickstart

Everything uses `vk_`:

```bash
# 1) Register, get an account-level vk_ (or POST /v1/accounts/login with an existing account)
curl -sX POST $BASE/v1/accounts -H 'content-type: application/json' \
  -d '{"name":"acme","email":"ops@acme.com","password":"<pw>"}'
# → data: { "account_id": "...", "api_key": "vk_..." }

ACCOUNT_KEY=vk_...

# 2) Create a Vector Workspace (server bootstraps a default dataset + Milvus collection)
curl -sX POST $BASE/v1/workspaces \
  -H "authorization: Bearer $ACCOUNT_KEY" -H 'content-type: application/json' \
  -d '{"name":"prod-index","kind":"db"}'
# → data: { "id": "<ws_id>", "kind": "db", "status": "active", ... }

WS=<ws_id>

# 3) (optional) Mint a service token scoped to this workspace for a business app
curl -sX POST $BASE/admin/v1/tokens \
  -H "authorization: Bearer $ACCOUNT_KEY" -H 'content-type: application/json' \
  -d "{\"app_id\":\"search-svc\",\"name\":\"prod\",\"allowed_workspaces\":[\"$WS\"]}"
# → data: { "id": "...", "token": "vk_..." }   # token shown only once

# 4) Write / search
curl -sX POST $BASE/v1/vectors/upsert \
  -H "authorization: Bearer $ACCOUNT_KEY" -H 'content-type: application/json' \
  -d "{\"workspace_id\":\"$WS\",\"records\":[{\"id\":\"sku-1\",\"text\":\"Air Jordan 1\"}]}"
```

> ⚠️ Anonymous onboarding (`POST /v1/accounts/anonymous`) creates a **File Workspace** (`kind=fs`). A Vector Workspace must be created explicitly with `{"kind":"db"}` in step 2.

---

## 4. Concepts

- **Vector Workspace (`kind=db`)**: equivalent to one Pinecone index — one workspace, one dedicated Milvus collection. Creation commits the workspace + a `default` dataset in a single transaction, then provisions the collection (rolled back on failure).
- **Dataset**: a logical grouping inside a workspace (`products`, `faq`, …), sharing the collection and separated by a scalar `dataset` field. Every Vector Workspace ships a `default` dataset that **cannot be deleted** (it's the implicit fallback when `dataset` is omitted).
- **Record**: one row. `text` is required (also BM25-indexed); `id` / `category` / `tags` / `meta` are optional. The vector is computed server-side from `text`. The physical primary key `{dataset}:{id}` is assembled server-side and never appears on the wire.

---

## 5. Data models (wire fields)

**Workspace**: `id` / `account_id` / `name` / `status` (`active`\|`archived`) / `kind` (`fs`\|`db`) / `app_id`? / `created_at` / `updated_at` (RFC3339)

**Dataset**: `id` / `workspace_id` / `name` / `status` / `created_at` / `updated_at` (RFC3339)

**VectorSearchHit** (`/v1/vectors/search`):

| Field | Type | Notes |
|---|---|---|
| `id` | string | record id (no dataset prefix) |
| `dataset` | string | owning dataset |
| `category` | string | category |
| `tags` | string[] | tags |
| `text` | string | original text |
| `meta` | object | custom JSON |
| `created_at` / `updated_at` | int64 | ms epoch |
| `score` | float | COSINE similarity, **higher = closer** |

**VectorRecordHit** (`/v1/vectors/query`): same, but **no `score`**.

---

## 6. Data-plane endpoints

Common params: `workspace_id`? (omission rules in §2), `dataset`? (defaults to `default`). Target must be a Vector Workspace (`kind=db`).

### POST `/v1/vectors/upsert`
Insert or full-row replace by `(dataset, id)`. Max **500** records per call.
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
Every field but `text` has a default: `id`→server UUID (**insert-only, non-idempotent**, see §10), `category`→`"default"`, `tags`→`[]`, `meta`→`{}`.

Response: `{ "ids": ["sku-1"], "commit_ts": 1735689600000 }`
- `ids`: ids actually written, in request order, **after same-batch dedupe by id** (last-wins), so possibly shorter than `records`. For omitted-`id` records this is the **first and only** place the server-generated UUID is surfaced — capture it.
- `commit_ts`: server completion time (ms epoch), good for same-server read-your-writes.
- Errors: `400 INVALID_INPUT` (field validation, incl. per-field length/charset overflow), `413 PAYLOAD_TOO_LARGE` (**only** records count > 500), `500 EMBEDDING_FAILED`.

### POST `/v1/vectors/search`
Dense ANN over the embedded `query`, **implicitly scoped to the target dataset** (no cross-dataset search in v0).
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
`top_k` defaults to 10, max 100; `filter` see §8. Response: `{ "hits": VectorSearchHit[] }`.
- Errors: `400 INVALID_INPUT` (query empty or > 65535 bytes / top_k=0 / bad filter), `413 PAYLOAD_TOO_LARGE` (top_k > 100), `500 EMBEDDING_FAILED`.

### POST `/v1/vectors/query`
Direct lookup by id. **Order not guaranteed; missing ids are silently skipped.** Max **500** ids.
```json
{ "workspace_id": "<ws_id>", "dataset": "products", "ids": ["sku-1","sku-2"] }
```
Response: `{ "hits": VectorRecordHit[] }`.

### POST `/v1/vectors/delete`
Hard-delete by id. Max **500** ids.
```json
{ "workspace_id": "<ws_id>", "dataset": "products", "ids": ["sku-1","sku-2"] }
```
Response: `{ "delete_count": 2 }`.
> ⚠️ `delete_count` is Milvus's tombstone count = `len(ids)`, unrelated to how many rows actually existed. To tell the difference, `query` first.

---

## 7. Control-plane endpoints

| Method | Path | Purpose |
|---|---|---|
| POST | `/v1/accounts` | Register → `{account_id, api_key}` |
| POST | `/v1/accounts/login` | Log in for a fresh `vk_` |
| POST | `/v1/workspaces` | Create workspace (`{"kind":"db"}` for a Vector Workspace), returns Workspace |
| GET | `/v1/workspaces` | List workspaces (paginated, see §11) |
| DELETE | `/v1/workspaces/{id}` | Soft-delete workspace (200 + `{success:true}`) |
| POST | `/v1/workspaces/{ws}/datasets` | Create dataset (201, returns Dataset) |
| GET | `/v1/workspaces/{ws}/datasets` | List datasets (paginated) |
| DELETE | `/v1/workspaces/{ws}/datasets/{name}` | Soft-delete dataset (204); `default` cannot be deleted (incl. case variants, returns 400) |
| POST | `/admin/v1/tokens` | Mint a `vk_` service token (201, `token` returned once) |
| POST | `/admin/v1/tokens/{id}/disable` | Revoke a token (204) |

**Mint a service token** `POST /admin/v1/tokens`:
```json
{ "app_id": "search-svc", "name": "prod",
  "allowed_workspaces": ["<ws_id>"], "expires_at": 1767225600000 }
```
`allowed_workspaces`? omitted = unrestricted within the account; `expires_at`? is ms epoch, omitted = never expires. Response `{ "id": "...", "token": "vk_..." }`.

---

## 8. Filter DSL (v0)

A strict subset of Qdrant-style, used only by `/v1/vectors/search`'s `filter`:

- Only `must` (no `should` / `must_not`); all clauses are AND-combined and merged with the base scope (`dataset == "X" && status == "active"`).
- `field` must be `meta.<key>`, key matching `[a-zA-Z0-9_-]+`, **no nesting** (`meta.a.b` errors). Platform fields (`dataset`/`tags`/`status`) aren't filterable through this DSL.
- `op`: `eq` \| `in` \| `gt` \| `gte` \| `lt` \| `lte`.
- `value`:
  - `eq`: scalar string / number / **bool**;
  - range `gt`/`gte`/`lt`/`lte`: **number / string only** (bool / null / array / object → 400);
  - `in`: array of scalars, **non-empty and ≤ 100 items** (expanded to an OR-chain at parse time).

```json
{ "must": [
  { "field": "meta.brand", "op": "in",  "value": ["nike", "adidas"] },
  { "field": "meta.price", "op": "gte", "value": 500 }
] }
```

---

## 9. Limits & validation

| Subject | Limit |
|---|---|
| `dataset` / `id` | `[a-zA-Z0-9_-]+`, no `:`; each ≤ 64 bytes (composite `{dataset}:{id}` ≤ 128) |
| `text` | non-empty, ≤ 65535 bytes (UTF-8); chunk client-side if larger |
| `meta` | any JSON value (object recommended — filter only works on `meta.<key>`), ≤ 16 KB serialized |
| `tags` | ≤ 8, each ≤ 128 bytes, no empty string |
| `category` | non-empty, ≤ 64 bytes |
| `upsert.records` / `query.ids` / `delete.ids` | non-empty, ≤ 500 / call |
| `search.top_k` | default 10, max 100 |
| `filter` `in` array | non-empty, ≤ 100 |
| list `limit` | default 100, max 200 (silently clamped) |

**Per-field overflow (length / charset / non-empty) → `400 INVALID_INPUT`**; **only batch-count overflow (records / ids > 500, top_k > 100) → `413 PAYLOAD_TOO_LARGE`**.

---

## 10. Idempotency & retries

**upsert idempotency depends on whether you supply `id`:**

| Mode | `id` | Retrying the same request |
|---|---|---|
| Supplied | caller-provided | **Idempotent**: same `(workspace, dataset, id)` is replaced in place; `created_at`/`updated_at` reset, all fields overwritten with the latest payload |
| Omitted | server UUID | **Not idempotent**: each retry creates a new record with a different UUID; network retries duplicate writes |

> Retry-prone callers **must supply their own `id`** (content hash or client-side UUIDv7).

**Same-batch duplicate `id`**: server-side dedupe, **last-wins** (last occurrence's value, first occurrence's position), no error; `ids` reflects the deduped result.

---

## 11. Pagination

`GET /v1/workspaces` and `GET /v1/workspaces/{ws}/datasets` use cursor pagination:
- `limit`: items per page, default 100, max 200 (silently clamped).
- `after`: the previous page's `next_cursor`, an opaque string.

```json
{ "success": true, "data": {
  "items": [ /* ... */ ], "has_more": true, "next_cursor": "<pass as next after>"
} }
```
`next_cursor` is absent when `has_more=false`. Order is stable but implementation-defined (row id ASC), not a business-meaningful sort.

---

## 12. Error codes

Failures are always `{ "success": false, "error_code": "...", "error": "..." }`. **Match on `error_code` only.**

| `error_code` | HTTP | Meaning |
|---|---:|---|
| `INVALID_INPUT` | 400 | Generic validation failure (charset / length / missing field / per-field overflow); `error` carries `<field>: <reason>` |
| `WORKSPACE_KIND_MISMATCH` | 400 | A vector endpoint hit a File Workspace (or vice versa) |
| `CANNOT_DELETE_DEFAULT_DATASET` | 400 | Refused to delete the `default` dataset (incl. case variants) |
| `UNAUTHORIZED` | 401 | Missing / invalid / expired bearer token |
| `PERMISSION_DENIED` | 403 | Authenticated, but `allowed_workspaces` doesn't cover the target, or you touched another account's resource |
| `NOT_FOUND` | 404 | workspace / dataset / token doesn't exist or is archived |
| `ALREADY_EXISTS` | 409 | dataset name collision (case-insensitive) / email already registered |
| `PAYLOAD_TOO_LARGE` | 413 | Only batch-count overflow (records / ids > 500, top_k > 100) |
| `EMBEDDING_FAILED` | 500 | Server-side embedding upstream error |
| `INTERNAL` | 500 | Catch-all for storage / unexpected errors |
