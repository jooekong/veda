# Vectors API (db-kind workspaces)

Pinecone-style data plane for company apps. v0 contract — designs locked
in [`docs/vectors-merge-plan.md`](../vectors-merge-plan.md); known gaps
tracked in [`docs/vectors-merge-backlog.md`](../vectors-merge-backlog.md).

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

All endpoints take `Authorization: Bearer <vk_…>`. Tokens scope to an
account; `allowed_workspaces` on the token restricts which db workspaces
the bearer can touch. Issue tokens via `POST /admin/v1/tokens`.

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
Same-batch duplicate `id` → last entry wins (Milvus PK upsert semantics).

```json
{
  "workspace_id": "...",         // optional if token scope = exactly 1 ws
  "dataset": "products",         // optional, default "default"
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

`ids` is the same length and order as `records` in the request. For
auto-generated UUIDs (records omitting `id`), this is the **only** place
to discover them — capture them client-side before they're needed for
query/delete.

`commit_ts` is server-now ms — Milvus REST doesn't expose the real timestamp.
Under synchronous semantics, this is sufficient for read-your-writes on the
same server.

### POST `/v1/vectors/search`

Dense ANN over the embedded `query`. Always implicitly scoped to the
target dataset (v0 has no cross-dataset search). Optional `filter` (see
below) AND-merges with the base scope.

```json
{
  "workspace_id": "...",
  "dataset": "products",       // optional
  "query": "sneakers under 1500",
  "top_k": 10,                 // default 10, max 100
  "filter": {                  // optional
    "must": [
      {"field": "meta.price", "op": "lt", "value": 1500},
      {"field": "meta.category", "op": "eq", "value": "shoes"}
    ]
  }
}
```

Each hit returns `id / dataset / category / tags / text / meta / created_at
/ updated_at / score` (COSINE distance).

### POST `/v1/vectors/query`

Direct lookup by `id`. Order not preserved; missing ids silently absent
(no error). Max 500 ids per call.

```json
{"workspace_id": "...", "dataset": "products",
 "ids": ["sku-1", "sku-2"]}
```

### POST `/v1/vectors/delete`

Hard-deletes by `id`. Returns Milvus's `accepted_count` — the number of
delete markers the engine created (for an `id in [...]` filter this equals
`len(ids)`; per Milvus 2.6 REST `data.deleteCount` semantics). Max 500
per call.

```json
{"workspace_id": "...", "dataset": "products",
 "ids": ["sku-1", "sku-2"]}
```

## Filter DSL (v0)

Strict subset of Qdrant-style:

- Only `must` (no `should` / `must_not`).
- Only `meta.<top_level_key>` paths. Platform fields (`dataset`, `tags`,
  …) are not filterable through this DSL — `dataset` is part of the base
  scope and others are unexposed.
- Operators: `eq`, `in`, `gt`, `gte`, `lt`, `lte`.
- `in` is parser-expanded to an OR-chain: `(meta["x"] == "a" || meta["x"]
  == "b")`. Don't rely on Milvus 2.6 TermExpr support over JSON paths.

## Control plane

| Method | Path | Purpose |
|---|---|---|
| POST | `/v1/workspaces` | Create workspace (`kind=db` for vector workspaces). For `kind=db` the server also bootstraps a `default` dataset and the Milvus collection. |
| GET | `/v1/workspaces` | List active workspaces (paginated) |
| POST | `/v1/workspaces/{ws}/datasets` | Create a new dataset in a db workspace |
| GET | `/v1/workspaces/{ws}/datasets` | List active datasets (paginated) |
| DELETE | `/v1/workspaces/{ws}/datasets/{name}` | Soft-delete (`status='archived'`). Cannot delete `default`. |
| POST | `/admin/v1/tokens` | Mint a `vk_` token scoped to the caller's account |
| POST | `/admin/v1/tokens/{id}/disable` | Revoke a token (ownership-checked) |

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
| `UNAUTHORIZED` | 401 | Missing / invalid bearer token |
| `PERMISSION_DENIED` | 403 | Authenticated but the token's `allowed_workspaces` doesn't cover the target |
| `NOT_FOUND` | 404 | Workspace / dataset / record / token doesn't exist |
| `ALREADY_EXISTS` | 409 | Dataset name collision (case-insensitive per MySQL collation) |
| `PRECONDITION_FAILED` | 412 | Conditional request lost the race (fs-side only) |
| `PAYLOAD_TOO_LARGE` | 413 | Batch / field exceeds documented limit |
| `QUOTA_EXCEEDED` | 429 | Vectors API does not return this currently; fs and SQL paths may (workspace storage cap / scan limit) |
| `EMBEDDING_FAILED` | 500 | Server-side embedding upstream error |
| `INTERNAL` | 500 | Catch-all for storage / deadlock / unexpected — opaque on purpose |

Charset and size limits:
- `dataset` / `id`: `[a-zA-Z0-9_-]+`, must not contain `:` (PK separator)
- `dataset` ≤ 64 bytes, `id` ≤ 64 bytes (composite physical pk `{dataset}:{id}` ≤ 128 bytes)
- `text` ≤ 65535 bytes UTF-8 (Milvus VARCHAR hard cap), `meta` (JSON-serialized) ≤ 16 KB, `tags` ≤ 8 entries × 128 bytes each
