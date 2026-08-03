# Veda Java SDK

A hand-written Java client for the Veda **db-workspace vector data-plane** —
the Pinecone-style `/v1/vectors/*` API. Four operations: `upsert`, `search`,
`query`, `delete`.

- **Scope**: data-plane only. Control-plane (workspace/dataset/token admin) and
  fs-workspace features are out of scope — provision your db workspace and get a
  `wk_` workspace key for it first (the platform/console does this with a `vk_`
  account key; your app only ever holds the `wk_`). See
  [`docs/api/db-workspace-api.md`](../../docs/api/db-workspace-api.md).
- **Baseline**: Java 8, Maven. Dependencies: Jackson (`jackson-databind`),
  OkHttp, and `com.csoss:monitor-instrumentation` for observability (pulls
  grpc / protobuf / snappy transitively — see [Observability](#observability)).
  No Lombok, no code generation.
- **Forward-compatible**: unknown response fields are ignored, unknown
  `error_code`s map to `UNKNOWN` — a newer server never breaks an older SDK.

> **Coordinates:** `csoss.veda:veda-sdk-java:0.0.1-RELEASE`, published to the
> internal ddxq Nexus (see `<distributionManagement>` in `pom.xml`). Credentials
> are read from your `~/.m2/settings.xml` (`ddmc.repo` / `ddmc.repo.snapshot` servers).

## Install

The SDK is published to the internal ddxq Nexus (coordinates above) — depend on
it directly:

```xml
<dependency>
  <groupId>csoss.veda</groupId>
  <artifactId>veda-sdk-java</artifactId>
  <version>0.0.1-RELEASE</version>
</dependency>
```

Alternatively, when the internal Nexus isn't an option, build and install it
into your local `~/.m2`:

```bash
cd sdk/java
mvn install
```

## Quickstart

```java
import csoss.veda.sdk.VedaClient;
import csoss.veda.sdk.filter.Filter;
import csoss.veda.sdk.model.*;

try (VedaClient veda = VedaClient.builder()
        .baseUrl("http://localhost:3000")
        .apiKey("wk_...")            // workspace key (data-plane), not an account vk_
        .workspaceId("ws-...")       // optional now — the wk_ already binds the workspace
        .dataset("products")         // optional; default = server's "default"
        .build()) {

    // upsert — supply your own id if you might retry (see "Idempotency")
    UpsertResult up = veda.upsert(UpsertRequest.builder()
        .addRecord(Record.builder()
            .id("sku-1").text("Air Jordan 1")
            .category("shoes").tags("sale", "new")
            .meta("price", 1299)
            .build())
        .build());
    up.getIds();        // ids written (deduped; may be shorter than the request)
    up.getCommitTs();   // epoch ms

    // search
    SearchResult s = veda.search(SearchRequest.builder()
        .query("sneakers under 1500")
        .topK(10)
        .filter(Filter.must()
            .lt("meta.price", 1500)
            .eq("meta.category", "shoes"))
        .outputFields("text", "meta")   // optional projection
        .build());
    for (SearchHit h : s.getHits()) {
        h.getId(); h.getScore(); h.getScoreType();   // see "Search modes"
        h.metaAsMap();
    }

    // query by id (also supports outputFields) / delete by id
    QueryResult q = veda.query(QueryRequest.builder().ids("sku-1", "sku-2").build());
    DeleteResult d = veda.delete(DeleteRequest.builder().ids("sku-1", "sku-2").build());
    d.getDeleteCount();   // == ids.length (tombstone count, NOT rows removed)
}
```

`VedaClient` is thread-safe; build one and share it. It owns an OkHttp
connection pool — `close()` it (or use try-with-resources) when done.

**Base URL.** `http://localhost:3000` above is a server you run yourself
(veda-server listens on `0.0.0.0:3000` by default). Against the shared data
plane use the deployed entry points instead:

| environment | base URL |
|---|---|
| production | `https://veda.ddmc-inc.com` |
| test | `https://veda.dbpaas.dingdongxiaoqu.com` |

## Scope: workspaceId / dataset

- `dataset` omitted → server uses `default` (safe). Can be overridden per request.
- `workspaceId`: the `wk_` key itself is bound to exactly one workspace, and the
  server's data-plane DTOs no longer have a `workspace_id` field. The SDK still
  injects one into request bodies; the server silently ignores it (adapting the
  SDK to the `wk_` model is a todo). Setting it is still useful as the
  `workspace` label on this SDK's spans/metrics (see
  [Observability](#observability)).

## Search modes

`mode` selects the ranker (server default `hybrid`). Scores are **not comparable
across modes** — always read `getScoreType()`.

| mode | embeds query? | `score_type` | range |
|---|---|---|---|
| `HYBRID` (default) | yes | `rrf` | ~[0, 0.033] |
| `SEMANTIC` | yes | `cosine` | ~[0, 1] |
| `FULLTEXT` | no | `bm25` | ~[0, 30+] |

`minScore` is a relevance floor applied **after** `top_k` (so results may be
fewer than `top_k`). It only works with `SEMANTIC`/`FULLTEXT`; sending it with
`HYBRID` (incl. the default) is a `400 INVALID_INPUT` — RRF is a rank artifact,
not a calibrated relevance. For a cosine threshold there is no universal "0.5":
even unrelated text scores ~0.15–0.25, so an effective floor is meaningfully
higher (e.g. 0.4–0.6). Calibrate by embedding a few known-unrelated pairs first.

> `mode` / `minScore` require a server with sparse-vector support.

## Filter DSL (v0)

`Filter.must()` builds an AND-only filter. The builder enforces the cheap rules
locally; the server is the source of truth for the rest:

- `field` must be `meta.<key>` — a single, non-nested key matching `[a-zA-Z0-9_-]+`.
  Platform fields (`dataset`, `tags`, `status`, …) are **not** filterable here.
- ops: `eq`, `in`, `gt`, `gte`, `lt`, `lte`.
- `eq` takes a scalar (string/number/bool); range ops (`gt`/`gte`/`lt`/`lte`)
  take number/string only (bool/null/array/object → `INVALID_INPUT`); `in` takes
  a non-empty scalar array of ≤100 values.

```java
Filter.must()
    .in("meta.brand", "nike", "adidas")
    .gte("meta.price", 500);
```

## Idempotency & retry

`upsert` is idempotent **only when every record carries your own `id`**
(content hash or client-generated UUID) — same `(workspace, dataset, id)`
replaces the row in place. An **omitted `id`** makes the server mint a fresh
UUID on every call, so a network retry would duplicate the write.

The SDK enforces this boundary: it auto-retries `search`/`query`/`delete` and
fully-id'd `upsert`s, but an `upsert` batch containing **any** id-less record is
sent **exactly once** (no auto-retry). OkHttp's own connection-failure retry is
disabled so the SDK is the sole authority.

Retries use exponential backoff for: network/timeout errors, HTTP `5xx`
(`EMBEDDING_FAILED`/`INTERNAL`), and `429`. `4xx` (except `429`) is never
retried. Tune with `.maxRetries(n)` (default 2; 0 disables).

> **`write_mode` is not exposed by this SDK version.** The server's upsert body
> accepts `write_mode`: `upsert` (default — idempotent dedup by id) or `insert`,
> a ~3x fast path that skips Milvus's dedup and so requires the caller to
> guarantee id uniqueness. A repeated id under `insert` is **undefined
> behavior**: rows accumulate and are not reclaimed by compaction, and reads
> return an unspecified copy — so retry-prone or re-importable pipelines must
> stay on the default `upsert`. This SDK has
> no `writeMode` field, so every `upsert()` uses the server default. If you need
> the `insert` fast path — bulk import of freshly-minted ids — issue the raw
> `POST /v1/vectors/upsert` REST call yourself.

## Errors

Failures throw unchecked exceptions:

- `VedaApiException` — server returned an error envelope. Has `getErrorCode()`
  (`ErrorCode` enum), `getHttpStatus()`, `isRetryable()`. Match on `getErrorCode()`,
  never the message text.
- `VedaTransportException` — connect/timeout failure or a non-JSON response.

`ErrorCode`: `INVALID_INPUT`, `WORKSPACE_KIND_MISMATCH`,
`CANNOT_DELETE_DEFAULT_DATASET`, `UNAUTHORIZED`, `PERMISSION_DENIED`,
`NOT_FOUND`, `ALREADY_EXISTS`, `PAYLOAD_TOO_LARGE`, `QUOTA_EXCEEDED`,
`EMBEDDING_FAILED`, `INTERNAL`, and `UNKNOWN` (any code this SDK version does
not recognise).

## Limits (pre-validated client-side where cheap)

- `upsert.records` / `query.ids` / `delete.ids`: non-empty, ≤500 per call.
- `search.topK`: ≤100. `filter` `in`: ≤100.
- Field length / charset limits are validated server-side (`INVALID_INPUT`).

## Timeouts

`.connectTimeoutMs(10_000)` / `.requestTimeoutMs(60_000)` (defaults match the
Veda CLI/FUSE clients).

## Observability

Each call emits a trace span and metrics via the company monitor stack
(`com.csoss.monitor`, OTLP). The SDK **only emits** — it never calls
`MonitorInitializer`; your application already does. When monitoring isn't
initialized the emit path is a safe no-op, and instrumentation **never throws
into your call** — a telemetry failure can't fail an otherwise-successful
request. Disable per client with `.observability(false)`.

The span is standalone: it parents to the host's active span if there is one,
but the SDK does not push its own span into the current context, so it won't
reparent your downstream spans.

**Span** `veda.db.<op>`, attributes:

| attribute | meaning |
|---|---|
| `op` | `upsert` / `search` / `query` / `delete` |
| `workspace` / `dataset` | effective scope (per-request override → client default) |
| `req_count` | input rows (records / ids); for search, the requested `top_k` (omitted when `top_k` is unset) |
| `result_count` | **rows returned**: written ids / hits / `delete_count` |
| `mode` | search ranker (`hybrid` / `semantic` / `fulltext`); search only |
| `error_code` | the `ErrorCode` name, on failure |

Span status is set to `OK` / `ERROR`; failures also record the exception.

**Metrics:**

| metric | type | labels |
|---|---|---|
| `veda_db_op_total` | counter | `op`, `workspace`, `status` |
| `veda_db_op_latency_ms` | histogram | `op`, `workspace`, `status` |
| `veda_db_rows_total` | counter (+= `result_count`) | `op`, `workspace` |

`dataset` is deliberately **not** a metric label — it can be high-cardinality
and the stack caps metric series, silently dropping the overflow; read it off
the span instead.

**Cardinality budget:** `workspace` *is* a metric label, so series count is
roughly `workspaces × ops × statuses`. The stack caps a metric at ~200 series
and silently drops the rest — fine for a client bound to a few workspaces, but a
single process fanning out across many (≳25) workspaces can hit the cap; split
clients or use `.observability(false)` if that's your shape.

Requires `com.csoss:monitor-instrumentation` on the classpath (a hard dependency
of this SDK).

## Testing

- **Unit tests** (`mvn test`): pure logic — filter builder, JSON mapping,
  forward-compat, error mapping, client pre-validation. No network.
- **Integration tests** (`VedaClientIT`): hit a **real** Veda + Milvus +
  embedding stack (no mocks) — the release gate. Run manually before tagging:

```bash
VEDA_URL=https://veda.dbpaas.dingdongxiaoqu.com VEDA_API_KEY=wk_... VEDA_WS_ID=ws-... \
  mvn -P integration verify
```

(or `VEDA_URL=http://localhost:3000` against a server you run yourself).

They auto-skip when `VEDA_URL` is unset, so the default build and CI runners
without internal access stay green. **Run them before every SDK release.**

## Version

SDK versions are independent of the server version. This SDK targets db data-plane
API **`v0`** (`VedaClient.SUPPORTED_API`), server ≥ veda 0.1.x.
