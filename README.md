# Veda

English | [简体中文](README.zh-CN.md)

A programmable knowledge store that unifies filesystem, vector search, and SQL — one server, one API, one CLI. Write a file and it becomes searchable by meaning; create a collection and every row is auto-embedded; point SQL at any of it.

## The Problem

Building anything retrieval-heavy (an AI agent's memory, a RAG backend, an internal semantic search) means assembling the same stack every time: object storage for the documents, a chunking + embedding ETL pipeline you write and babysit yourself, a vector database, a full-text engine because vectors alone miss exact identifiers, a relational store for structured data, and key management for multi-tenancy. Each seam is yours to keep consistent — when a document changes, nothing re-embeds it unless you built that.

Veda collapses this pipeline into one service:

```bash
veda cp ./design.pdf /docs/design.pdf        # store (text or binary; PDF/Word get their text extracted)
veda search "why did we pick an outbox"      # hybrid semantic + BM25 search, a few seconds later
veda sql "SELECT path, size_bytes FROM files WHERE path LIKE '/docs/%'"
```

Chunking, embedding, indexing, and summarization happen server-side and asynchronously. Consistency between the file store and the vector index is the server's job, not yours: file writes and their sync tasks commit in a single MySQL transaction (outbox pattern), and a background worker keeps Milvus caught up.

## Use Cases

- **AI agent memory / knowledge base** — coding agents (Claude Code, Cursor, …) attach through the server-native MCP endpoint with one `.mcp.json` entry, or through the `veda` CLI (an agent-facing `skill.md` ships with the installer). Tiered summaries (L0 one-liner → L1 overview → full content) let an agent triage many files without burning tokens on full reads.
- **RAG backend** — upload documents (Markdown, source code, PDF/Word), get hybrid search with relevance scores over chunks — or a synthesized answer with inline citations via `/v1/answer` / `veda ask`; no separate ETL to operate.
- **Self-hosted vector database** — `kind=db` workspaces expose a Pinecone-style raw-vector data plane (upsert/search/query/delete + metadata filters) for apps that just need vectors, with a Java SDK available.
- **A filesystem you can grep by meaning** — mount a workspace with FUSE and edit it with vim/IDE like a local directory, while everything stays semantically indexed; or use `veda grep` for literal matches and `veda search` for conceptual ones.
- **Platform building block** — a gateway-facing surface (`/v1/workspace/{workspace}/project/...`) lets an AI platform embed veda as its storage layer, with auth externalized to the platform gateway.

## Features

- **Filesystem** — `cp`, `cat`, `ls`, `mv`, `rm`, `append`, `mkdir` over plain absolute paths. Text is chunked and indexed; binary files (PDF/Word/images/jars) are stored verbatim as blobs with real MIME types. PDF and Word files additionally get their text extracted and embedded, so the original stays byte-for-byte downloadable while its content becomes searchable.
- **Hybrid search** — every text file is automatically chunked, embedded, and BM25-indexed. Three modes: `hybrid` (dense + BM25 fused with RRF, default), `semantic`, `fulltext`. Chinese tokenization via jieba.
- **Tiered summaries** — an LLM generates an L0 abstract (~100 tokens) and L1 overview (~2k tokens) per file, aggregated bottom-up for directories. Search can return any tier via `detail_level`.
- **Structured collections** — schema-first tables with one auto-embedded field; insert JSON rows, search them semantically, filter them with SQL.
- **Vector workspaces** — a raw-vector data plane (`kind=db`) for apps that bring their own records: upsert/search/query/delete with metadata filters and `write_mode=insert` for ~3× bulk-load throughput.
- **SQL** — an embedded DataFusion engine queries files and collections (`SELECT`, `WHERE`, `JOIN`, aggregates), plus UDFs for filesystem ops and vector search inside SQL.
- **FUSE mount** — `veda-fuse mount` exposes a workspace as a local directory: native tools just work, a write-back mode debounces editor noise (vim swap files, git lockfiles), and SSE keeps caches consistent with remote changes.
- **MCP endpoint** — the server speaks the Model Context Protocol natively (`POST /mcp`, Streamable HTTP, stateless): coding agents attach with one `.mcp.json` entry and get six read-only tools — search / grep / read_file / list_dir / overview / `ask` (one-shot RAG answer with citations).
- **Multi-tenant** — Account → Workspace hierarchy. Account key (`vk_`) drives the control plane; per-workspace keys (`wk_`, revocable, read-only variant available) drive the data plane. Plain key auth, no JWT.

## How It Works

```
    CLI (veda)     FUSE mount     REST / SSE      Platform gateway
        │              │              │                  │
        └──────────────┴──────┬───────┴──────────────────┘
                              │
                      veda-server (Axum)
              auth: vk_ (control) / wk_ (data plane)
                    ┌─────────┴─────────┐
                    │                   │
                  MySQL              Milvus
             (control plane)      (data plane)
             ┌──────────────┐   ┌────────────────────────┐
             │ accounts     │   │ veda_chunks            │
             │ workspaces   │   │   dense + BM25 sparse  │
             │ dentries     │   │ veda_summaries         │
             │ files        │   │   L0 abstracts         │
             │ file_blobs   │   │ veda_coll_{id}         │
             │ summaries    │   │   structured rows      │
             │ outbox       │   │ ws_<hash>_default      │
             │ datasets     │   │   raw vectors (db ws)  │
             │ schemas      │   └────────────────────────┘
             └──────────────┘
                    │
             Worker (tokio task, outbox consumer)
             chunk → embed → summarize (LLM) → index
```

| Component | Technology | Role |
|-----------|-----------|------|
| HTTP layer | Rust, Axum | REST API, SSE events, auth middleware |
| Control plane | MySQL 8 | Accounts, keys, path tree, file content, outbox task queue (ACID) |
| Data plane | Milvus 2.5+ | Dense ANN + BM25 sparse vectors, RRF hybrid search |
| SQL engine | DataFusion (Arrow) | Embedded, queries files + collections, no extra service |
| Embedding / LLM | Any OpenAI-compatible API | Chunk embeddings, L0/L1 summaries (LLM optional) |
| FUSE client | fuser | Local mount with read cache + write-back buffering |
| Observability | Prometheus + OTLP bridge | `/v1/metrics` exporter, optional OTLP gRPC push |

### Key Design Decisions

- **Outbox pattern for consistency**: a file write and its sync tasks (ChunkSync / SummarySync / ExtractSync) commit in one MySQL transaction; a background worker replays them into Milvus. Eventual consistency by default, on-demand drift reconcile (`POST /admin/v1/reconcile/{ws}`) for disasters. Lease fencing makes the outbox safe for multiple servers sharing one MySQL.
- **Storage tiers by content**: UTF-8 text ≤256KB inline, >256KB chunked; non-UTF-8 stored as blobs with MIME sniffed from magic bytes. Content-addressed dedup (SHA256) skips writes when nothing changed.
- **Search that admits keyword search matters**: dense vectors for meaning, BM25 for identifiers and exact terms, RRF to fuse them — per-query mode selection instead of pretending one ranker fits all.
- **Tiered context loading**: L0/L1 summaries are first-class stored objects (searchable in Milvus, not computed on read), designed for agents that need to triage before they read.
- **Two workspace kinds, one auth model**: `fs` workspaces carry files + collections + summaries; `db` workspaces carry only raw vectors. Both authenticate data-plane requests with a single-lookup `wk_` key check.
- **Simplicity bias**: MySQL over Kafka, single binary over microservices, plain keys over JWT.

## Quick Start

### Prerequisites

- Rust toolchain (server build)
- MySQL 8.0+
- Milvus 2.5+ (hybrid search relies on the BM25 Function, absent in 2.4.x; the bundled compose pins v2.5.5, also verified against 2.6.14)
- An OpenAI-compatible embedding API
- Optional: an OpenAI-compatible chat API for L0/L1 summaries (feature auto-disables without it)

### 1. Start dependencies

```bash
# MySQL + Milvus (etcd/minio come along via depends_on)
cd deploy && cp .env.example .env   # set passwords + embedding key
docker compose up -d mysql milvus
cd ..
```

Prefer everything in containers? `docker compose up -d` builds and runs the
full single-host stack (dependencies + veda-server + Prometheus) — then skip
steps 2 and 3.

### 2. Configure

The server reads `config/server.toml` by default, or takes a config path as
its only positional argument (`veda-server /etc/veda/config.toml`).

```bash
cp config/test.toml.example config/server.toml   # then edit
```

```toml
listen = "0.0.0.0:3000"   # optional — this is the default

[mysql]
database_url = "mysql://root:password@localhost:3306/veda"

[milvus]
url = "http://localhost:19530"

[embedding]
api_url = "https://api.openai.com/v1/embeddings"
api_key = "sk-your-key"
model = "text-embedding-3-small"
dimension = 1024

# Optional — enables L0/L1 summaries
# [llm]
# api_url = "https://api.openai.com/v1/chat/completions"
# api_key = "sk-your-key"
# model = "gpt-4o-mini"
```

Values can be overridden via `VEDA_*` environment variables. The MySQL schema
bootstraps itself on first start (`CREATE TABLE IF NOT EXISTS`, no migration step).

### 3. Build and run

```bash
cargo build --release
./target/release/veda-server
```

### 4. Install the CLI

Build from source (`cargo build --release -p veda-cli`), or pull a prebuilt
binary from a running server — every server serves its own installer:

```bash
curl -fL http://<your-server>/install.sh | sh        # add --with-fuse for the FUSE client
```

### 5. Create account and workspace

```bash
# Zero-input anonymous onboard: server mints account + default workspace + keys
veda init

# Or register a named account in the same command
veda init --email joe@example.com --password 'something-strong'
```

Need an extra workspace? `veda workspace add <alias>`. Importing a key from
another machine? `veda init --import-key vk_…` (auto-backs up old config).

## Usage

### File Operations

```bash
# Upload (local → remote; "-" reads stdin). Text and binary both work:
# the server sniffs UTF-8 — text is chunked + indexed, binary is stored
# as a blob, PDFs get their text layer extracted and indexed.
veda cp ./README.md /docs/readme.md
veda cp ./design.pdf /docs/design.pdf
veda cp -r ./src /code                   # recursive directory upload

# Browse
veda ls /docs
veda cat /docs/readme.md
veda cat /docs/readme.md --range 10:20   # line slice; also --head N / --tail N
veda cat /docs/design.pdf > local.pdf    # binary round-trips byte-for-byte

# Organize
veda mv /docs/old.md /archive/old.md
veda rm /tmp                             # delete file or directory (recursive)
```

### Search, Grep, Summaries

```bash
# Hybrid search (semantic + BM25, default)
veda search "how does authentication work"

# Single-mode
veda search "error handling patterns" --mode semantic
veda search "TODO fix" --mode fulltext

# Scope to a subtree / control result granularity
veda search "auth" --path /docs
veda search "outbox" --detail-level abstract    # return L0 summaries instead of chunks

# Literal substring scan (synchronous, no embedding lag)
veda grep "TODO" /src

# Tiered summaries (LLM-generated, async)
veda abstract /docs/design.pdf    # L0 — one sentence
veda overview /docs               # L1 — structured overview, directories aggregate bottom-up
```

### Structured Collections

```bash
# Create — schema is a JSON array; --embed-source picks the auto-embedded field
veda collection create articles \
  --schema '[{"name":"title","type":"string","index":true},
             {"name":"content","type":"string"},
             {"name":"category","type":"string","index":true}]' \
  --embed-source content

# Insert — a JSON ARRAY of rows (auto-embeds the content field)
veda collection insert articles \
  '[{"title":"Intro to Rust","content":"Rust is a systems...","category":"tech"}]'

# Semantic search against the embedded field
veda collection search articles "systems programming" --limit 10

# Filters / aggregates live in SQL
veda sql "SELECT title, category FROM articles WHERE category = 'tech' LIMIT 5"
```

### FUSE Mount

```bash
veda-fuse mount ~/veda-mount             # uses the CLI config's active workspace
vim ~/veda-mount/docs/notes.md           # native tools just work
cat ~/veda-mount/docs/.abstract          # read-only summary sidecars per directory
veda-fuse umount ~/veda-mount
```

Default is daemon mode with a read cache and SSE-driven invalidation.
`--write-mode=writeback` buffers writes locally (5s debounce) so editor
temp files never reach the server.

### Vector Workspaces (Pinecone-style)

Veda also offers a raw-vector data plane on `kind=db` workspaces — designed
for apps that need vector storage without the file abstraction.
Schema, defaults, and contracts: [`docs/api/vectors.md`](docs/api/vectors.md).

```bash
# Control plane — account key (vk_, held by the platform/console): create a
# db-kind workspace, then mint a workspace key (wk_) for it. Your app only ever
# holds the wk_; vk_ stays on the platform side.
curl -sS -X POST http://localhost:3000/v1/workspaces \
  -H "Authorization: Bearer $VK" \
  -H "Content-Type: application/json" \
  -d '{"name":"my-vectors","kind":"db","app_id":"my-app"}'

# Data plane — workspace key (wk_). The target workspace is bound to the key, so
# the request body carries NO workspace_id. text is required; rest has defaults.
curl -sS -X POST http://localhost:3000/v1/vectors/upsert \
  -H "Authorization: Bearer $WK" \
  -H "Content-Type: application/json" \
  -d '{"records":[
        {"id":"sku-1","text":"Air Jordan 1","meta":{"price":1299}},
        {"id":"sku-2","text":"Yeezy 350","meta":{"price":1599}}]}'

# Search — mode defaults to hybrid; pick semantic/fulltext explicitly. No
# workspace_id in the body.
curl -sS -X POST http://localhost:3000/v1/vectors/search \
  -H "Authorization: Bearer $WK" \
  -H "Content-Type: application/json" \
  -d '{"query":"sneakers under 1500","mode":"semantic","top_k":5,
       "filter":{"must":[{"field":"meta.price","op":"lt","value":1500}]}}'
```

Java SDK: [`sdk/java`](sdk/java) (`upsert`/`search`/`query`/`delete`, typed
exceptions, idempotency-aware retry). Python example:
[`examples/python_pinecone_demo.py`](examples/python_pinecone_demo.py).

## Project Structure

```
veda/
├── crates/
│   ├── veda-types/      # Domain types, error definitions (zero dep)
│   ├── veda-core/       # Traits + business logic (no storage impl)
│   ├── veda-store/      # MySQL + Milvus implementations
│   ├── veda-pipeline/   # Embedding, chunking, PDF text extraction, LLM summaries
│   ├── veda-sql/        # DataFusion SQL engine
│   ├── veda-server/     # Axum HTTP server (thin shell) + outbox worker
│   ├── veda-cli/        # CLI client (binary name: veda)
│   └── veda-fuse/       # FUSE mount (workspace member; install via --with-fuse)
├── sdk/java/            # Java SDK for the db-workspace data plane
├── web/                 # Landing page + user docs site + admin console
├── deploy/              # Dockerfile, docker-compose, systemd units
├── docs/
│   ├── api/             # API contracts (db-workspace, vectors)
│   ├── plans/           # Active plans (index: docs/design/plans.md)
│   ├── testing/         # Test SOPs
│   └── archive/         # Completed / superseded docs
├── ARCHITECTURE.md      # Current system state
└── AGENTS.md            # Agent working protocol
```

## Search Modes

`POST /v1/vectors/search` (db workspaces) and `veda search` (fs) take a `mode`:

| Mode | How it works | `score_type` | Best for |
|------|-------------|------------|----------|
| **hybrid** (default) | Vector + BM25, fused with RRF | `rrf` | General purpose |
| **semantic** | Cosine similarity | `cosine` | Conceptual search |
| **fulltext** | BM25 keyword | `bm25` | Exact terms, identifiers |

Scores aren't comparable across `score_type`. `min_score` (a relevance floor)
applies only to `semantic`/`fulltext`; passing it with `hybrid` returns 400
(RRF rank isn't a relevance score). Full contract:
[`docs/api/db-workspace-api.md`](docs/api/db-workspace-api.md).

## Status

Alpha. Minor versions can break compatibility — see [`CHANGELOG.md`](CHANGELOG.md).
Not yet implemented: image OCR, K8s Helm chart.

## License

MIT
