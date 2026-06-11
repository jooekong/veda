# Vector Workspace API

A **Vector Workspace** is a Veda workspace of `kind=db`: Pinecone-style managed vector retrieval — write text → the server embeds it automatically → retrieve by semantic / full-text search, with meta filtering. Built for business apps integrating over REST API / SDK.

> As opposed to a **File Workspace** (`kind=fs`, file storage + CLI/FUSE) — the two have different data models and access paths and don't interoperate. This page is the field-by-field contract for Vector Workspaces; for the full picture of architecture / auth / control plane, see the [full reference](#/docs/reference).

---

## 1. Conventions

- HTTP + JSON (UTF-8), `Content-Type: application/json`. Business paths live under `/v1/*` or `/admin/v1/*`. `<BASE>` is deployment-defined (example: `https://veda.dbpaas.dingdongxiaoqu.com`); examples below use `$BASE`.
- **Response envelope**: success `{ "success": true, "data": {…} }`; failure `{ "success": false, "error_code": "INVALID_INPUT", "error": "text: must not be empty" }`. `error_code` is a stable machine-readable code — **clients match on it only, never parse the `error` text**. Some delete endpoints return `204 No Content` with no body.
- **Two time formats**: control-plane objects (Workspace / Dataset `created_at` / `updated_at`) are **RFC3339 strings**; vector hit `created_at` / `updated_at`, `upsert`'s `commit_ts`, and token-mint `expires_at` are **int64 ms epoch**. Don't unify them when deserializing.

---

## 2. Authentication (read this first)

Credentials go in `Authorization: Bearer <token>`. A Vector Workspace has two planes, each with its own key type:

| Plane | Endpoints | Credential | Prefix | Scope |
|---|---|---|---|---|
| **Data plane** | `/v1/vectors/*` | **workspace key** | `wk_` | Bound to a single db workspace; `read` or `readwrite` |
| **Control plane** | `/v1/workspaces*`, `/v1/workspaces/{ws}/datasets*`, `/v1/workspaces/{id}/keys*` | **account key** | `vk_` | Account-level (all workspaces under the account) |

- **The data plane accepts only `wk_`**: the target workspace is bound by the key, so `/v1/vectors/*` request bodies carry **no `workspace_id`**. A read-only `wk_` can `search` / `query` but not `upsert` / `delete` (→ `403 PERMISSION_DENIED`).
- **The control plane accepts only `vk_`**: create workspaces, manage datasets, issue `wk_`. The `vk_` stays with the platform / console and is **never handed to business consumers** — a business app typically holds exactly one `wk_`.
- Using a `vk_` on `/v1/vectors/*`, or a `wk_` on the control plane, is rejected with `401 UNAUTHORIZED`. JWT has been removed entirely — there is no third credential type.

---

## 3. Integration flow

The control plane (create the workspace + issue the `wk_`) uses `vk_` and is handled by the platform / console; the business app runs the data plane with the `wk_` it receives.

```bash
# —— Control plane (platform side, holds the account vk_) ——
ACCOUNT_KEY=vk_...

# 1) Create a db workspace (server bootstraps the default dataset + Milvus collection)
curl -sX POST $BASE/v1/workspaces \
  -H "authorization: Bearer $ACCOUNT_KEY" -H 'content-type: application/json' \
  -d '{"name":"prod-index","kind":"db"}'
# → data: { "id": "<ws_id>", "kind": "db", "status": "active", ... }
WS=<ws_id>

# 2) Issue a data-plane wk_ for this workspace and hand it to the business app
curl -sX POST $BASE/v1/workspaces/$WS/keys \
  -H "authorization: Bearer $ACCOUNT_KEY" -H 'content-type: application/json' \
  -d '{"name":"search-svc","permission":"readwrite"}'
# → data: { "key": "wk_...", "permission": "readwrite" }   # plaintext shown only this once — store it safely

# —— Data plane (business app, holds only the wk_; request bodies carry no workspace_id) ——
WK=wk_...
curl -sX POST $BASE/v1/vectors/upsert \
  -H "authorization: Bearer $WK" -H 'content-type: application/json' \
  -d '{"records":[{"id":"sku-1","text":"Air Jordan 1"}]}'
```

> ⚠️ **Don't use anonymous onboarding for db**: `POST /v1/accounts/anonymous` issues an account + workspace in one step, but it creates **`kind=fs`** — unusable with the vector endpoints. A db workspace must be created explicitly via step 1 with `{"kind":"db"}`.

---

## 4. Concepts

- **Workspace (`kind=db`)**: equivalent to one Pinecone index — one workspace, one dedicated Milvus collection. Creation commits the workspace + a `default` dataset in a single transaction, then provisions the collection (rolled back on failure).
- **Dataset**: a logical grouping inside a workspace (`products`, `faq`, …), sharing the same collection and separated by the scalar `dataset` field. Every db workspace ships a `default` dataset that **cannot be deleted** (the implicit fallback when `dataset` is omitted).
- **Record**: one row. `text` is required (the server computes the vector and builds the BM25 index from it); `id` / `category` / `tags` / `meta` are optional. The physical primary key `{dataset}:{id}` is assembled server-side and **never appears on the wire**.

---

## 5. Data models (wire fields)

> Fields marked `?` may be absent. Time formats: see §1.

**Workspace**: `id` / `account_id` / `name` / `status` (`active`\|`archived`) / `kind` (`fs`\|`db`) / `app_id`? / `description`? / `created_at` / `updated_at` (RFC3339)

**Dataset**: `id` / `workspace_id` / `name` / `status` / `description`? / `created_at` / `updated_at` (RFC3339)

**WorkspaceKey** (list item of `GET /v1/workspaces/{id}/keys`): `id` / `workspace_id` / `account_id` / `name` / `permission` (`read`\|`readwrite`) / `status` (`active`\|`revoked`) / `kind` / `created_at` (RFC3339). The plaintext `wk_` is returned only once, at creation.

**VectorSearchHit** (`/v1/vectors/search` hit):

| Field | Type | Notes |
|---|---|---|
| `id` | string | record id (no dataset prefix), **always returned** |
| `score` | float | relevance score, **higher = more relevant**, **always returned**; meaning depends on `score_type` |
| `score_type` | string | `cosine` (semantic ANN, ~[0,1]) / `bm25` (full-text, ~[0,30+]) / `rrf` (hybrid fusion, ~[0,0.033]). **Not comparable across types** |
| `dataset` | string | owning dataset |
| `category` | string | category |
| `tags` | string[] | tags |
| `text` | string | original text |
| `meta` | object | custom JSON |
| `created_at` / `updated_at` | int64 | ms epoch |

> `dataset` and everything below it are **projectable fields**: without `output_fields` all of them are returned; with `output_fields` only the listed ones are (`id` / `score` / `score_type` are always returned — no need to list them).

**VectorRecordHit** (`/v1/vectors/query` hit): same, but with **no `score` / `score_type`** (direct lookup by id, not a ranked match).

---

## 6. Endpoint reference (🟦 `wk_`)

The target workspace for all four endpoints is bound by the `wk_`; the only common param is `dataset`? (defaults to `default` when omitted). `upsert` / `delete` require `readwrite`.

### POST `/v1/vectors/upsert`
Insert or full-row replace by `(dataset, id)`. Max **500** records per call.

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

- Every record field but `text` has a default: `id`→server UUID (**goes through insert, non-idempotent**, see §9), `category`→`"default"`, `tags`→`[]`, `meta`→`{}`.
- Top-level `write_mode`?: `"upsert"` (default, idempotent and safe) / `"insert"` (skips the dedupe lookup, ~3x faster, **caller guarantees pk uniqueness**; duplicate pks are undefined behavior in Milvus, see §9).

Response 200: `{ "ids": ["sku-1"], "commit_ts": 1735689600000 }`
- `ids`: ids actually written, in request order, **after same-batch dedupe by id** (last-wins), so possibly shorter than `records`. For omitted-`id` records this is the **first and only** place the server-generated UUID is surfaced — capture it.
- `commit_ts`: server completion time (ms epoch), for same-server read-your-writes (not a distributed, comparable logical clock).
- Errors: `400 INVALID_INPUT` (field validation, incl. per-field length / charset overflow, empty `records`), `403 PERMISSION_DENIED` (read-only `wk_`), `404 NOT_FOUND` (dataset missing / archived), `413 PAYLOAD_TOO_LARGE` (**only** `records` count > 500), `500 EMBEDDING_FAILED`.

### POST `/v1/vectors/search`
Search the target dataset (**implicitly scoped** — no cross-dataset search in v0).

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

| `mode` | Behavior | Embeds query? | `score_type` | Score range |
|---|---|:--:|---|---|
| `hybrid` (**default**) | dense ANN + BM25, RRF fusion | yes | `rrf` | ~[0, 0.033] |
| `semantic` | dense ANN over the embedded query | yes | `cosine` | ~[0, 1] |
| `fulltext` | BM25 over tokenized `text` | no | `bm25` | ~[0, 30+] |

- `mode`? defaults to `hybrid`; `top_k`? defaults to 10, max 100; `filter`? see §7.
- `output_fields`?: projection whitelist, a subset of `{dataset, category, tags, text, meta, created_at, updated_at}`. Omitted = return all; `id` / `score` / `score_type` are always returned — **don't list them** (an invalid field in the list → `400`).
- `min_score`?: **relevance floor** — hits below it are dropped. **Only valid with `semantic` (cosine) / `fulltext` (bm25)**; combining it with `hybrid` (including the default mode) is a `400` — RRF is a ranking, not a relevance measure; for a threshold use `top_k` or an explicit `mode=semantic`. Applied after `top_k`, so results may be **fewer than `top_k`**. Calibrate per model (with dense embeddings, unrelated text often scores ~0.15–0.25, so a useful threshold is noticeably higher, e.g. 0.4–0.6 — there is no universal "0.5").
- A failed `hybrid` errors out — it does **not silently degrade to semantic**. `fulltext` skips the embedding call (cheaper, and the only mode that doesn't depend on the embedding model).

Response 200: `{ "hits": VectorSearchHit[] }`.
- Errors: `400 INVALID_INPUT` (query empty or > 65535 bytes / `top_k=0` / bad `filter` / invalid field in `output_fields` / `min_score` not finite or combined with `hybrid`), `404 NOT_FOUND` (dataset), `413 PAYLOAD_TOO_LARGE` (`top_k` > 100), `500 EMBEDDING_FAILED` (semantic/hybrid embedding failure).

### POST `/v1/vectors/query`
Direct lookup by id. **Order not guaranteed; missing ids are silently skipped (no error).** Max **500** ids per call.

```json
{ "dataset": "products", "ids": ["sku-1","sku-2"], "output_fields": ["text"] }
```

Response 200: `{ "hits": VectorRecordHit[] }` (no `score`). `output_fields`? same as search.
- Errors: `400 INVALID_INPUT` (empty `ids` / invalid id), `404 NOT_FOUND` (dataset), `413 PAYLOAD_TOO_LARGE` (> 500).

### POST `/v1/vectors/delete`
Hard-delete by id. Max **500** ids per call.

```json
{ "dataset": "products", "ids": ["sku-1","sku-2"] }
```

Response 200: `{ "delete_count": 2 }`.
> ⚠️ `delete_count` is the number of **tombstones Milvus created = `len(ids)`**, **unrelated** to how many rows actually existed and got deleted. To tell the difference, `query` first.
- Errors: `400 INVALID_INPUT` (empty `ids` / invalid id), `403 PERMISSION_DENIED` (read-only `wk_`), `404 NOT_FOUND` (dataset), `413 PAYLOAD_TOO_LARGE` (> 500).

---

## 7. Filter DSL (v0)

A strict subset of Qdrant-style filtering, used only by `/v1/vectors/search`'s `filter`:

- Only `must` (no `should` / `must_not`); all clauses are **AND**-combined and merged with the base scope (`dataset == "X" && status == "active"`).
- `field` must be `meta.<top-level key>`, key matching `[a-zA-Z0-9_-]+`, **no nesting** (`meta.a.b` → 400). Platform fields (`dataset` / `tags` / `status`, …) are **not filterable through this DSL**.
- `op`: `eq` \| `in` \| `gt` \| `gte` \| `lt` \| `lte`.
- `value`: `eq` accepts scalar string / number / bool; range ops `gt`/`gte`/`lt`/`lte` accept **only** number / string (bool / null / array / object → 400); `in` takes an array of scalars, **non-empty and ≤ 100 items**.

```json
{ "must": [
  { "field": "meta.brand", "op": "in",  "value": ["nike", "adidas"] },
  { "field": "meta.price", "op": "gte", "value": 500 }
] }
```

---

## 8. Limits & validation

| Subject | Limit |
|---|---|
| `dataset` / `id` charset | `[a-zA-Z0-9_-]+`; `:` forbidden (the physical PK separator) |
| `dataset` | non-empty, ≤ 64 bytes |
| `id` | non-empty, ≤ 64 bytes (and the composite `{dataset}:{id}` ≤ 128 bytes) |
| `text` | non-empty, ≤ 65535 bytes (UTF-8, Milvus VARCHAR hard cap); chunk client-side if larger |
| `meta` | ≤ 16 KB JSON-serialized; object recommended (filter only works on `meta.<key>`) |
| `tags` | ≤ 8 items, each non-empty and ≤ 128 bytes |
| `category` | non-empty, ≤ 64 bytes |
| `upsert.records` | non-empty, ≤ 500 / call |
| `query.ids` / `delete.ids` | non-empty, ≤ 500 / call |
| `search.top_k` | default 10, max 100 |
| `filter` `in` array | non-empty, ≤ 100 |
| list `limit` | default 100, max 200 (silently clamped) |

All field validation completes **before** any Milvus write. **Per-field overflow (length / charset / non-empty) → `400 INVALID_INPUT`** (`error` reads `<field>: <reason>`); **only batch-count overflow (`records` / `ids` > 500, `top_k` > 100) → `413 PAYLOAD_TOO_LARGE`**.

---

## 9. Idempotency & retries

Write semantics are determined by `write_mode` (default `upsert`) together with whether you supply an `id`:

| `write_mode` | `id` | Behavior | Idempotent |
|---|---|---|:--:|
| `upsert` (default) | supplied | full-row in-place replace by `(workspace, dataset, id)`; timestamps reset, `meta`/`tags`/`text` fully overwritten | ✅ |
| `upsert` (default) | omitted | server UUID → insert fast path (UUIDs can't collide) | ❌ |
| `insert` | supplied | straight insert, **skips the dedupe lookup**, ~3x faster; **caller guarantees `id` uniqueness** | ❌ |
| `insert` | omitted | server UUID → insert | ❌ |

> **`insert` trades safety for speed**: Milvus insert doesn't check pk uniqueness — duplicate pks are **undefined behavior**: multiple rows physically accumulate, compaction does **not** clean them up automatically, and which one `query`/`search` returns is unknown. Use it only where ids are inherently unique (auto-increment / UUID / one-shot import). **Pipelines that retry or may re-import must use the default `upsert`**.

- **Same-batch duplicate `id`**: server-side dedupe, **last-wins** (last occurrence's value, first occurrence's position), no error; the response `ids` reflects the deduped result.
- **Mixed batches are not atomic**: a default-upsert request mixing with-`id` and without-`id` records is split into two Milvus calls (no atomicity). For safe retries, supply `id` on every record in the batch.
- **commit_ts**: an approximation from the server's local clock, only for same-server read-your-writes.

---

## 10. Error codes

Failures are always `{ "success": false, "error_code": "...", "error": "..." }`. **Match on `error_code` only.**

| `error_code` | HTTP | Meaning |
|---|---:|---|
| `INVALID_INPUT` | 400 | Validation failure (charset / length / missing field / per-field overflow / invalid `output_fields` / misused `min_score`); `error` carries `<field>: <reason>` |
| `WORKSPACE_KIND_MISMATCH` | 400 | A vector endpoint hit an fs workspace (or vice versa) |
| `CANNOT_DELETE_DEFAULT_DATASET` | 400 | Refused to delete the `default` dataset |
| `UNAUTHORIZED` | 401 | Missing / invalid / expired / wrong-plane token — incl. a `wk_` revoked because its workspace was archived |
| `PERMISSION_DENIED` | 403 | Read-only `wk_` calling `upsert`/`delete`; or a `vk_` touching another account's resource |
| `NOT_FOUND` | 404 | workspace / dataset / key / token doesn't exist or is archived |
| `ALREADY_EXISTS` | 409 | workspace / dataset name collision (case-insensitive) / email already registered |
| `PAYLOAD_TOO_LARGE` | 413 | **Only** batch-count overflow (`records`/`ids` > 500, `top_k` > 100) |
| `QUOTA_EXCEEDED` | 429 | Not currently returned by the vector API; reserved |
| `EMBEDDING_FAILED` | 500 | Server-side embedding upstream error (the `error` text is scrubbed to `internal server error`; only `error_code` is stable) |
| `INTERNAL` | 500 | Catch-all for storage / deadlock / unexpected errors; details intentionally withheld |

---

## 11. Control-plane endpoints (🔑 `vk_`)

The lifecycle of data-plane `wk_` keys and datasets lives on the control plane, using the account `vk_`:

| Method | Path | Purpose |
|---|---|---|
| POST | `/v1/accounts` | Register an account → `{ account_id, api_key }` |
| POST | `/v1/accounts/login` | Log in for a fresh `vk_` |
| POST | `/v1/workspaces` | Create a workspace (`{"kind":"db"}`) |
| GET | `/v1/workspaces` | List workspaces (paginated) |
| DELETE | `/v1/workspaces/{id}` | Soft-delete a workspace (200) |
| POST | `/v1/workspaces/{id}/keys` | Issue a data-plane `wk_` (`read`/`readwrite`, plaintext shown once) |
| GET | `/v1/workspaces/{id}/keys` | List `wk_` metadata (not paginated) |
| DELETE | `/v1/workspaces/{id}/keys/{key_id}` | Revoke a `wk_` (204) |
| POST | `/v1/workspaces/{ws}/datasets` | Create a dataset (201) |
| GET | `/v1/workspaces/{ws}/datasets` | List datasets (paginated) |
| DELETE | `/v1/workspaces/{ws}/datasets/{name}` | Soft-delete a dataset (204; `default` cannot be deleted) |
| POST | `/admin/v1/tokens` | Mint a scoped `vk_` service token (201, `token` returned once) |
| POST | `/admin/v1/tokens/{id}/disable` | Revoke a token (204) |

Full request / response fields, the apps platform plane, and ops endpoints: see the [full reference](#/docs/reference).

---

## 12. Before going live

- **Embedding throughput is hard-capped by the cloud vendor's QPM limit**, with no client-side concurrency gate: for bulk writes / high-concurrency search, throttle concurrency and batch with backoff, or use `mode=fulltext` for pure-keyword cases (no embedding involved).
- **Write throughput << read throughput**: for bulk writes prefer `write_mode=insert` (when ids are unique) + ≤ 500 records / call.
- Soft-deleting a workspace / dataset does **not reclaim Milvus vectors**.
- The Java SDK still implements the old `vk_` contract and is **not adapted to `wk_`**; for now, integrate over raw HTTP as described on this page.
