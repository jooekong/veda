# Vectors API (db-kind workspaces)

> **Authoritative external contract:** `web/public/docs/zh/reference.md` +
> `web/public/docs/zh/vectors.md` (published with the console). This file is an
> in-repo reference for agents / SDK development; where the two disagree, the
> web version wins.

Pinecone-style data plane for company apps. v0 contract — designs locked
in [`docs/archive/vectors-merge-plan.md`](../archive/vectors-merge-plan.md); open items live in
[`docs/plans/db-workspace-followups.md`](../plans/db-workspace-followups.md)
(the original backlog is archived at
[`../archive/vectors-merge-backlog.md`](../archive/vectors-merge-backlog.md)).

## Concepts

- **Workspace (`kind=db`)**: a Pinecone-style "index" — one per workspace.
  Created via `POST /v1/workspaces { kind: "db" }`. The server provisions
  a per-workspace Milvus collection and bootstraps a default `dataset`.
- **Dataset**: logical grouping within a workspace (`"products"`, `"faq"`,
  etc.). Shares the workspace's collection; rows are separated by a scalar
  `dataset` field. Every db workspace ships with a `default` dataset.
- **Record**: one row. Has a unique `id` (within its dataset), a `text`
  (required, indexed for BM25), an optional dense `vector` (server computes
  from `text`), a JSON `meta`, plus optional `category` / `tags`. Server
  composes a physical `{dataset}:{id}` primary key internally — the wire
  contract never exposes it.

## Auth

All four data-plane endpoints take `Authorization: Bearer <wk_…>` — a
**workspace key** bound to exactly one db workspace (internal
`AuthDbWorkspace`). The target workspace is derived from the key, so request
bodies do **not** carry `workspace_id`. A read-only `wk_` may `search` /
`query` but not `upsert` / `delete` (→ `403 PERMISSION_DENIED`). Keys are
issued by the control plane (`POST /v1/workspaces/{id}/keys`) and handed to
the app by the platform; the account key `vk_` is **not** used on the data
plane.

## Defaults

`upsert` accepts highly defaulted records — the only required field is
`text`. The server fills:

| Field | Default | Notes |
|---|---|---|
| `dataset` (body top-level) | `"default"` | The bootstrapped dataset |
| `id` | server UUID | Insert-only when omitted (each retry creates a new record). **Retry-prone callers must supply their own `id`** to get upsert (insert-or-replace by id) semantics |
| `category` | `"default"` | Mid-level taxonomy |
| `tags` | `[]` | Multi-value labels |
| `meta` | `{}` | Free-form JSON, ≤16KB |
| `created_at` / `updated_at` | server-now | Both reset on every upsert (Pinecone-style; same `(workspace, dataset, id)` collision = full replace) |

## Endpoints

### POST `/v1/vectors/upsert`

Inserts or replaces records by `(dataset, id)`. Max 500 records per call.
Same-batch duplicate `id` is server-side deduped (last entry wins) — see
**Idempotency** below.

```json
{
  "dataset": "products",         // optional, default "default"
  "write_mode": "upsert",        // optional, "upsert"(default) | "insert"
  "records": [
    {"id": "sku-1", "text": "Air Jordan 1",
     "category": "shoes", "tags": ["sale","new"],
     "meta": {"price": 1299}}
  ]
}
```

Response:
```json
{"success": true, "data": {
  "ids": ["sku-1"],
  "commit_ts": 1735689600000
}}
```

`ids` echoes the request order, **minus any duplicates collapsed by
server-side dedupe** (see Idempotency below). For auto-generated UUIDs
(records omitting `id`), this is the **only** place to discover them —
capture them client-side before they're needed for query/delete.

`commit_ts` is server-now ms — Milvus REST doesn't expose the real timestamp.
Under synchronous semantics, this is sufficient for read-your-writes on the
same server.

#### Write mode & idempotency

`write_mode` (default `upsert`) and whether `id` is supplied together decide the semantics:

| `write_mode` | `id` | Behavior | Idempotent |
|---|---|---|---|
| `upsert` (default) | supplied | insert-or-replace by `(workspace,dataset,id)` — replaced in place; `created_at`/`updated_at` reset, meta/tags/text fully overwritten | ✅ |
| `upsert` (default) | omitted | server UUID → internally takes the **insert** fast path (a UUID can't collide) | ❌ (new UUID each retry) |
| `insert` | supplied | direct insert, **dedup skipped** — ~3× faster (Milvus upsert pays a ~400ms dedup+delete cost; see `docs/archive/loadtest-2026-06-05.md`). **Caller guarantees `id` uniqueness.** | ❌ |
| `insert` | omitted | server UUID → insert | ❌ |

**`write_mode=insert` trades safety for speed.** Milvus does NOT check pk
uniqueness on insert: a repeated `pk` is **undefined behavior** — it physically
accumulates rows that compaction does NOT auto-reclaim (no tombstone; only a
later delete/upsert on that pk lets compaction remove old rows, so the bloat
persists), and `query`/`search` return an **unspecified copy**. Use it
ONLY when ids are inherently unique (autoincrement / UUID / one-shot import).
**Retry-prone or re-imported pipelines MUST use the default `upsert`** — only
upsert has deterministic last-write-wins (insert new + delete old).

Within a single upsert call, duplicate `id` is **server-side deduped,
last-wins**: the later record in the `records` array overrides earlier
ones; earlier copies are dropped before embedding so you don't pay for
vectors you won't store. (Milvus itself rejects same-batch duplicate
PKs with error code 1100, so the server collapses before submitting.)
There is no error for duplicate ids in the same batch — the response
`ids` reflects the deduped count, which may be shorter than the
request's `records` length.

Example — request body:
```json
{"records": [
  {"id": "A", "text": "first",  "meta": {"v": 1}},
  {"id": "B", "text": "other",  "meta": {"v": 1}},
  {"id": "A", "text": "winner", "meta": {"v": 2}}
]}
```
Response: `{"ids": ["A", "B"], "commit_ts": ...}`. After this call the
stored `A` row has `text: "winner"` and `meta: {"v": 2}`.

### POST `/v1/vectors/search`

Search the target dataset (v0 has no cross-dataset search). Optional `filter`
(see below) AND-merges with the base scope. `mode` selects the ranker:

| `mode` | what it does | embeds query? | `score_type` | score range |
|---|---|---|---|---|
| `hybrid` (**default**) | dense ANN + BM25 fused by RRF | yes | `rrf` | ~[0, 0.033] |
| `semantic` | dense ANN over the embedded query | yes | `cosine` | ~[0, 1] |
| `fulltext` | BM25 full-text over the analyzed `text` | no | `bm25` | ~[0, 30+] |

Scores are **not comparable across modes** — read `score_type` before reasoning
about magnitude. `fulltext` skips the embedding call entirely (cheaper, and the
only mode that works without an embedding model). `hybrid` failures surface as
errors (no silent fallback to semantic).

`min_score` is an optional **relevance floor**: hits scoring below it are
dropped. It applies **only to `semantic` (cosine) / `fulltext` (bm25)** — sending
it with `hybrid` (including the default mode) returns `400`, because the RRF
score is a rank artifact, not a relevance value (use `top_k`, or `mode=semantic`,
for a gate). It is applied *after* `top_k`, so the response may contain fewer
than `top_k` hits (raise `top_k` to surface more above the floor). Calibrate per
model: with dense embeddings even unrelated text scores ~0.15–0.25, so a useful
cosine floor sits well above that (e.g. 0.4–0.6) — there is no universal "0.5".

```json
{
  "dataset": "products",       // optional
  "query": "sneakers under 1500",
  "mode": "semantic",          // hybrid (default) | semantic | fulltext
  "top_k": 10,                 // default 10, max 100
  "min_score": 0.4,            // optional; semantic/fulltext only (400 on hybrid)
  "output_fields": ["text", "meta"],  // optional projection; omit = all fields
  "filter": {                  // optional
    "must": [
      {"field": "meta.price", "op": "lt", "value": 1500},
      {"field": "meta.category", "op": "eq", "value": "shoes"}
    ]
  }
}
```

Each hit returns `id / dataset / category / tags / text / meta / created_at
/ updated_at / score / score_type`.

#### `output_fields` (projection)

Optional whitelist selecting which **projectable** fields come back:

```
dataset | category | tags | text | meta | created_at | updated_at
```

- **Omitted → every field is returned** (the full hit shape above).
- Present → only the listed fields are returned; unselected ones are absent
  from the JSON entirely (not `null`), so SDK hit types must model them as
  optional. An empty array is legal and means "id/score/score_type only".
- `id`, `score` and `score_type` are **always** returned and must **not** be
  listed. Listing them — or any internal column (`pk`, `vector`,
  `sparse_vector`, `status`, `expire_at`) — is a `400 INVALID_INPUT`.
  (`id` is always fetched from Milvus regardless of the projection.)
- Validation happens **before** the embedding call, so a bad projection fails
  fast without spending an embed.

### POST `/v1/vectors/query`

Direct lookup by `id`. Order not preserved; missing ids silently absent
(no error). Max 500 ids per call.

```json
{"dataset": "products", "ids": ["sku-1", "sku-2"], "output_fields": ["text"]}
```

`output_fields` is optional here too, with the same whitelist and semantics
as `search` (omit = all fields; `id` always returned). This endpoint has no
`score` / `score_type`, so there is nothing score-shaped to list.

### POST `/v1/vectors/delete`

Hard-deletes by `id`. Returns `delete_count` — Milvus's number of delete
markers created (mirrored from REST `data.deleteCount`). The server deletes
with a `pk in [...]` filter (`pk` is the internal composite `{dataset}:{id}`,
built by `build_pk_in_filter`), so `delete_count` always equals `len(ids)`
regardless of which ids physically existed; it is **not** "rows that existed
and were removed". Use `query` first if you need to distinguish. Max 500 per
call.

```json
{"dataset": "products", "ids": ["sku-1", "sku-2"]}
```

Response:
```json
{"success": true, "data": {"delete_count": 2}}
```

## Filter DSL (v0)

Strict subset of Qdrant-style:

- Only `must` (no `should` / `must_not`).
- Only `meta.<top_level_key>` paths. Platform fields (`dataset`, `tags`,
  …) are not filterable through this DSL — `dataset` is part of the base
  scope and others are unexposed.
- The meta key must be non-empty and match `[a-zA-Z0-9_-]+`. Nested paths
  (`meta.a.b`) are rejected — one top-level key only.
- Operators: `eq`, `in`, `gt`, `gte`, `lt`, `lte`.
- `eq` takes a JSON **scalar** (string / number / bool). Arrays, objects and
  `null` are rejected.
- Range ops (`gt` / `gte` / `lt` / `lte`) take **only a number or a string**.
  bool / `null` / array / object are rejected (Milvus comparison semantics on
  bool/null are undefined).
- `in` takes an array of scalars: **non-empty and ≤100 items** (an empty array
  matches nothing and is almost always a caller bug; the cap bounds the
  generated expression).
- `in` is parser-expanded to an OR-chain: `(meta["x"] == "a" || meta["x"]
  == "b")`. Don't rely on Milvus 2.6 TermExpr support over JSON paths.

Every violation above is a `400 INVALID_INPUT`.

## Control plane

Workspace / dataset / key management uses the account key `vk_`
(`AuthAccount`) — held by the platform/console, not the data-plane app. Mint
a data-plane `wk_` with `POST /v1/workspaces/{id}/keys` and hand it to the app.

| Method | Path | Purpose |
|---|---|---|
| POST | `/v1/workspaces` | Create workspace (`kind=db` for vector workspaces). For `kind=db` the server also bootstraps a `default` dataset and the Milvus collection. |
| GET | `/v1/workspaces` | List active workspaces (paginated) |
| DELETE | `/v1/workspaces/{id}` | Archive (soft-delete, `status='archived'`). Cascade-revokes **all** the workspace's `wk_` keys in the same transaction — subsequent data-plane calls return `401 UNAUTHORIZED` |
| POST | `/v1/workspaces/{id}/keys` | Issue a `wk_` data-plane key; `permission` = `read` \| `readwrite` (plaintext shown once) |
| GET | `/v1/workspaces/{id}/keys` | List `wk_` metadata (no plaintext) |
| DELETE | `/v1/workspaces/{id}/keys/{key_id}` | Revoke a `wk_` |
| POST | `/v1/workspaces/{ws}/datasets` | Create a new dataset in a db workspace |
| GET | `/v1/workspaces/{ws}/datasets` | List active datasets (paginated) |
| DELETE | `/v1/workspaces/{ws}/datasets/{name}` | Soft-delete (`status='archived'`). Cannot delete `default`. |

### Platform surface (AI Platform gateway)

Platform integration uses a **separate** set of routes with auth externalized
to the gateway instead of `vk_`. There is **no `/v1/apps/*` route** — don't
generate SDKs against that shape.

Terminology: `{workspace}` in the paths is the platform **tenant code**
(stored internally as `app_id`); a veda workspace underneath it is called a
**project** (`{id}` is the workspace id).

Control plane (`crates/veda-server/src/routes/apps.rs`):

| Method | Path | Purpose |
|---|---|---|
| GET | `/v1/my/projects` | The gateway user's projects, flattened across workspaces |
| POST / GET | `/v1/workspace/{workspace}/projects` | Create / list projects |
| GET / PATCH / DELETE | `/v1/workspace/{workspace}/project/{id}` | Get / update / delete a project |
| POST / GET | `/v1/workspace/{workspace}/project/{id}/keys` | Issue / list `wk_` data-plane keys |
| DELETE | `/v1/workspace/{workspace}/project/{id}/keys/{key_id}` | Revoke a key |
| GET | `/v1/workspace/{workspace}/project/{id}/keys/{key_id}/token` | Fetch a key's plaintext |
| POST / GET | `/v1/workspace/{workspace}/project/{id}/datasets` | Create / list datasets |

Data plane (`crates/veda-server/src/routes/project_data.rs`), same core logic
as the `wk_` endpoints above:

`POST /v1/workspace/{workspace}/project/{id}/vectors/{upsert|search|query|delete}`

**Different envelope.** The platform surface returns the company envelope, not
veda's `{success, data}`:

| Case | Body |
|---|---|
| list | `{ "data": [...], "page", "size", "order_by", "order", "total", "total_page", "has_next_page", "has_prev_page" }` |
| single object (create / update / getToken) | the object **bare** — no `data` wrapper |
| no content (delete / revoke) | `{}` |
| error | `{ "error": { "code", "reason", "message", "external" } }` (REST status unchanged) |

**Different pagination.** Offset-based, not the cursor scheme below: `page`
(from 1), `size` (default 20, max 200), `order_by` (`created_at` \| `id`,
default `created_at`), `order` (`asc` \| `desc`, default `desc`), `keyword`
(**accepted only on `GET /v1/my/projects`** — the other list endpoints parse
and discard it; case-insensitive substring match on a project's `name` **or**
`description`, with `%` / `_` acting as LIKE wildcards; blank = no filter).

The `vk_` plane above is the current direct-access form; `/admin/v1/tokens`
(scoped `vk_` minting) still exists for account-level service tokens.

### Pagination (GET list endpoints)

Both `GET /v1/workspaces` and `GET /v1/workspaces/{ws}/datasets` support
cursor pagination via query string:

- `limit` — items per page; default 100, max 200 (silently clamped)
- `after` — opaque cursor from the previous page's `next_cursor`

Response shape:
```json
{
  "success": true,
  "data": {
    "items": [...],
    "has_more": true,
    "next_cursor": "<opaque-id-to-pass-as-after-next-call>"
  }
}
```

`next_cursor` is omitted when `has_more` is `false`. Sort order is stable
across requests but implementation-defined (currently row id ASC,
UUID-lexicographic) — clients that need a specific sort should resort
client-side after fetching all pages.

## Error responses

Every error response has shape:
```json
{
  "success": false,
  "error_code": "<STABLE_CODE>",
  "error": "<human-readable message; wording may evolve, do not match on it>"
}
```

**Always match on `error_code`, not `error`.** Codes are stable; messages
are not.

| `error_code` | HTTP | Meaning |
|---|---:|---|
| `INVALID_INPUT` | 400 | Generic validation failure (charset, length, missing field). `error` carries `<field>: <reason>` |
| `WORKSPACE_KIND_MISMATCH` | 400 | Vector API called on an fs workspace, or fs API called on a db workspace |
| `CANNOT_DELETE_DEFAULT_DATASET` | 400 | `DELETE /v1/workspaces/{ws}/datasets/default` is refused; the implicit-fallback dataset is reserved |
| `INVALID_PATH` | 400 | Path-shaped input failed (fs-side only) |
| `UNAUTHORIZED` | 401 | Missing / invalid `wk_`, or wrong plane (`vk_` on the data plane) |
| `PERMISSION_DENIED` | 403 | Read-only `wk_` used for `upsert` / `delete` |
| `NOT_FOUND` | 404 | Workspace / dataset / record / token doesn't exist |
| `ALREADY_EXISTS` | 409 | Dataset name collision (case-insensitive per MySQL collation) |
| `PRECONDITION_FAILED` | 412 | Conditional request lost the race (fs-side only) |
| `PAYLOAD_TOO_LARGE` | 413 | Batch count exceeds limit (`records`/`ids` >500, `top_k` >100); single-field size overruns return `INVALID_INPUT` |
| `QUOTA_EXCEEDED` | 429 | Vectors API does not return this currently; fs and SQL paths may (workspace storage cap / scan limit) |
| `EMBEDDING_FAILED` | 500 | Server-side embedding upstream error |
| `INTERNAL` | 500 | Catch-all for storage / deadlock / unexpected — opaque on purpose |

Charset and size limits:
- `dataset` / `id`: `[a-zA-Z0-9_-]+`, must not contain `:` (PK separator)
- `dataset` ≤ 64 bytes, `id` ≤ 64 bytes (composite physical pk `{dataset}:{id}` ≤ 128 bytes)
- `text` ≤ 65535 bytes UTF-8 (Milvus VARCHAR hard cap), `meta` (JSON-serialized) ≤ 16 KB, `tags` ≤ 8 entries × 128 bytes each
- `category`: non-empty, ≤ 64 bytes
- `filter` `in` array: non-empty, ≤ 100 items
