# Veda

A programmable knowledge store that unifies filesystem, vector search, and SQL queries. Store files, search by meaning, query with SQL — all through one API.

## What is Veda?

Veda is the successor to vecfs, rebuilt from the ground up with multi-tenancy, structured collections, and a cleaner architecture:

- **Filesystem** — store text files via familiar `cp`, `cat`, `ls`, `rm` operations (PDF/OCR planned)
- **Vector search** — every file is automatically chunked, embedded, and indexed for semantic + full-text hybrid search
- **Structured collections** — create tables with auto-embedding, like a vector-native database
- **Vector workspaces** — a Pinecone-style raw-vector data plane (`kind=db`) for company apps
- **SQL queries** — query files and collections with DataFusion SQL engine
- **Multi-tenant** — Account → Workspace isolation; account key (`vk_`) drives the control plane, workspace key (`wk_`) the data plane

## Architecture

```
           CLI (veda)                REST API / SSE
               │                          │
               └────────┬────────────────┘
                        │
                  veda-server (Axum)
                  ┌─────┴─────┐
                  │           │
               MySQL       Milvus
          (control plane)  (data plane)
          ┌────────────┐  ┌───────────────────┐
          │ accounts   │  │ veda_chunks        │
          │ workspaces │  │   - fs chunks      │
          │ dentries   │  │   - BM25 index     │
          │ files      │  │ veda_summaries     │
          │ outbox     │  │ veda_coll_{id}     │
          │ datasets   │  │   - structured     │
          │ schemas    │  │ ws_<hash>_default  │
          └────────────┘  │   - db vectors     │
                  │       └───────────────────┘
          Embedding Worker
          (tokio task)
          OpenAI-compat
```

- **MySQL** — control plane: accounts, file metadata, path tree, outbox task queue (ACID)
- **Milvus** — data plane: chunked content + embeddings (ANN + BM25), structured collections, and one raw-vector collection per `kind=db` workspace
- **Embedding Worker** — background tokio task that consumes outbox events; drift repair is an on-demand admin endpoint (`POST /admin/v1/reconcile/{ws}`), not a background loop
- **DataFusion** — embedded SQL engine for querying files and collections

### Key Design Decisions

- **Layered file storage**: ≤256KB inline in MySQL `file_contents`, >256KB chunked in `file_chunks`
- **Content-addressed dedup**: SHA256 fingerprint skips writes when content hasn't changed
- **Outbox pattern**: file writes and their sync tasks commit in one MySQL transaction; Milvus catches up asynchronously
- **Text-first storage**: text files only in v0; PDF/OCR extraction planned, no binary blobs
- **Structured collections**: data stored directly in Milvus with synchronous embedding, MySQL only stores schema metadata

## Quick Start

### Prerequisites

- Rust toolchain
- MySQL 8.0+
- Milvus 2.6+ (BM25 hybrid search relies on Milvus functions; verified against 2.6.14)
- An OpenAI-compatible embedding API

### 1. Start dependencies

```bash
docker run -d --name mysql -e MYSQL_ROOT_PASSWORD=password -e MYSQL_DATABASE=veda -p 3306:3306 mysql:8
docker run -d --name milvus -p 19530:19530 milvusdb/milvus:latest standalone
```

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
```

Values can be overridden via `VEDA_*` environment variables. The MySQL schema
bootstraps itself on first start. For a full stack (Prometheus/Grafana
included) see `deploy/docker-compose.yml`.

### 3. Build and run

```bash
cargo build --release
./target/release/veda-server
```

### 4. Create account and workspace

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
# Upload (local → remote; "-" reads stdin)
veda cp ./README.md /docs/readme.md

# Browse
veda ls /docs
veda cat /docs/readme.md
veda cat /docs/readme.md --range 10:20   # line slice; also --head N / --tail N

# Download = cat into a local file
veda cat /docs/readme.md > ./readme.md

# Organize
veda mv /docs/old.md /archive/old.md
veda rm /tmp        # delete file or directory (directories delete recursively)
```

### Search

```bash
# Hybrid search (semantic + BM25, default)
veda search "how does authentication work"

# Semantic only
veda search "error handling patterns" --mode semantic

# Full-text only
veda search "TODO fix" --mode fulltext

# Scope to a subtree / literal substring scan
veda search "auth" --path /docs
veda grep "TODO" /src
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

### Vector Workspaces (Pinecone-style)

Veda also offers a Pinecone-style data plane on `kind=db` workspaces — designed
for company apps that need cheap raw-vector storage without the file abstraction.
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

Python client example: [`examples/python_pinecone_demo.py`](examples/python_pinecone_demo.py).

## Project Structure

```
veda/
├── crates/
│   ├── veda-types/      # Domain types, error definitions (zero dep)
│   ├── veda-core/       # Traits + business logic (no storage impl)
│   ├── veda-store/      # MySQL + Milvus implementations
│   ├── veda-pipeline/   # Embedding, chunking, text extraction (PDF/OCR planned)
│   ├── veda-sql/        # DataFusion SQL engine
│   ├── veda-server/     # Axum HTTP server (thin shell)
│   ├── veda-cli/        # CLI client (binary name: veda)
│   └── veda-fuse/       # FUSE mount (workspace member; install via --with-fuse)
├── sdk/java/            # Java SDK for the db-workspace data plane
├── web/                 # Console + user docs site
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

## License

MIT
