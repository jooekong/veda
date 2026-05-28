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
- **Record**: one row. Has a composite `pk = "{dataset}:{row_key}"`, a
  `text` (required, indexed for BM25), an optional dense `vector` (server
  computes from `text`), a JSON `meta`, plus optional `category` / `tags` /
  `status` / `expire_at`.

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
| `row_key` | server UUID | Means insert-only; supply your own for upsert dedup |
| `category` | `"default"` | Mid-level taxonomy |
| `tags` | `[]` | Multi-value labels |
| `status` | `"active"` | Search auto-filters `status=="active"` |
| `meta` | `{}` | Free-form JSON, ≤16KB |
| `expire_at` | `null` | Epoch ms; v0 doesn't auto-cleanup |
| `created_at` / `updated_at` | server-now | Both reset on every upsert (Pinecone-style; PK upsert is a full replace) |

## Endpoints

### POST `/v1/vectors/upsert`

Inserts or replaces records. Max 500 per call. PK-collision = full replace.

```json
{
  "workspace_id": "...",         // optional if token scope = exactly 1 ws
  "dataset": "products",         // optional, default "default"
  "records": [
    {"row_key": "sku-1", "text": "Air Jordan 1",
     "category": "shoes", "tags": ["sale","new"],
     "meta": {"price": 1299}}
  ]
}
```

Response:
```json
{"success": true, "data": {
  "inserted": [{"pk": "products:sku-1", "row_key": "sku-1"}],
  "commit_ts": 1735689600000
}}
```

`commit_ts` is server-now ms — Milvus REST doesn't expose the real timestamp.
Under synchronous semantics, this is sufficient for read-your-writes on the
same server.

### POST `/v1/vectors/search`

Dense ANN over the embedded `query`. Always implicitly filtered by
`status == "active"` and `dataset == "<name>"`; v0 has no cross-dataset
search. Optional `filter` (see below) AND-merges with the base.

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

Each hit returns `pk / row_key / dataset / category / tags / status / text /
meta / expire_at / created_at / updated_at / score` (COSINE distance).

### POST `/v1/vectors/query`

Direct lookup by composite PK. Order not preserved; missing PKs silently
absent. Max 500 row_keys per call.

```json
{"workspace_id": "...", "dataset": "products",
 "row_keys": ["sku-1", "sku-2"]}
```

### POST `/v1/vectors/delete`

Hard-deletes by composite PK. Returns the **accepted** count (PKs submitted
to Milvus), not the actually-deleted count — Milvus REST doesn't surface
that. Max 500 per call.

```json
{"workspace_id": "...", "dataset": "products",
 "row_keys": ["sku-1", "sku-2"]}
```

## Filter DSL (v0)

Strict subset of Qdrant-style:

- Only `must` (no `should` / `must_not`).
- Only `meta.<top_level_key>` paths. Platform fields (`dataset`, `status`,
  `tags`, …) are not filterable through this DSL — they're handled by the
  base filter.
- Operators: `eq`, `in`, `gt`, `gte`, `lt`, `lte`.
- `in` is parser-expanded to an OR-chain: `(meta["x"] == "a" || meta["x"]
  == "b")`. Don't rely on Milvus 2.6 TermExpr support over JSON paths.

## Control plane

| Method | Path | Purpose |
|---|---|---|
| POST | `/v1/workspaces` | Create workspace (`kind=db` for vector workspaces). For `kind=db` the server also bootstraps a `default` dataset and the Milvus collection. |
| POST | `/v1/workspaces/{ws}/datasets` | Create a new dataset in a db workspace |
| GET | `/v1/workspaces/{ws}/datasets` | List active datasets |
| DELETE | `/v1/workspaces/{ws}/datasets/{name}` | Soft-delete (`status='archived'`). Cannot delete `default`. |
| POST | `/admin/v1/tokens` | Mint a `vk_` token scoped to the caller's account |
| POST | `/admin/v1/tokens/{id}/disable` | Revoke a token (ownership-checked) |

## Validation contract

Stable error codes (HTTP body `error` field):
- `workspace_kind_mismatch` (400) — wrong API for the workspace's kind
- `cannot delete the default dataset` (400)
- `invalid input: <field>: <reason>` (400)
- `not found: <resource>` (404)
- `payload too large: <field>: <count> exceeds <limit>` (413)
- `already exists: dataset <name>` (409)
- `unauthorized` (401), `permission denied` (403)

Charset and size limits:
- `dataset` / `row_key`: `[a-zA-Z0-9_-]+`, must not contain `:` (PK separator)
- `dataset` ≤ 64 bytes, `row_key` ≤ 64 bytes, composite `pk` ≤ 128 bytes
- `text` ≤ 65535 bytes UTF-8 (Milvus VARCHAR hard cap), `meta` (JSON-serialized) ≤ 16 KB, `tags` ≤ 8 entries × 128 bytes each
- `status` ∈ {`"active"`, `"inactive"`}; search base filter pins to `"active"`
