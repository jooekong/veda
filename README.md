# Veda

A programmable knowledge store that unifies filesystem, vector search, and SQL queries. Store files, search by meaning, query with SQL — all through one API.

## What is Veda?

Veda is the successor to vecfs, rebuilt from the ground up with multi-tenancy, structured collections, and a cleaner architecture:

- **Filesystem** — store text/PDF/images via familiar `cp`, `cat`, `ls`, `rm` operations
- **Vector search** — every file is automatically chunked, embedded, and indexed for semantic + full-text hybrid search
- **Structured collections** — create tables with auto-embedding, like a vector-native database
- **SQL queries** — query files and collections with DataFusion SQL engine
- **Multi-tenant** — Account → Workspace isolation with API Key / Workspace Token auth

## Architecture

```
           CLI (veda)                REST API / WebSocket
               │                          │
               └────────┬────────────────┘
                        │
                  veda-server (Axum)
                  ┌─────┴─────┐
                  │           │
               MySQL       Milvus
          (control plane)  (data plane)
          ┌────────────┐  ┌─────────────────┐
          │ accounts   │  │ veda_chunks      │
          │ workspaces │  │   - file chunks  │
          │ dentries   │  │   - embeddings   │
          │ files      │  │   - BM25 index   │
          │ outbox     │  │                  │
          │ schemas    │  │ veda_coll_{id}   │
          └────────────┘  │   - structured   │
                  │       │                  │
          Embedding Worker│ veda_vectors_{d} │
          (tokio task)    │   - raw vectors  │
          OpenAI-compat   └─────────────────┘
```

- **MySQL** — control plane: accounts, file metadata, path tree, outbox task queue (ACID)
- **Milvus** — data plane: chunked content + embeddings (ANN + BM25), structured collections, raw vectors
- **Embedding Worker** — background tokio task, processes outbox events, self-healing reconciler
- **DataFusion** — embedded SQL engine for querying files and collections

### Key Design Decisions

- **Layered file storage**: ≤256KB inline in MySQL `file_contents`, >256KB chunked in `file_chunks`
- **Content-addressed dedup**: SHA256 fingerprint skips writes when content hasn't changed
- **Outbox pattern**: MySQL outbox table replaces `semantic_tasks` for Milvus eventual consistency
- **No S3**: PDF/images store extracted text only, no original binary storage
- **Structured collections**: data stored directly in Milvus with synchronous embedding, MySQL only stores schema metadata

## Quick Start

### Prerequisites

- Rust toolchain (1.75+)
- MySQL 8.0+
- Milvus 2.4+
- An OpenAI-compatible embedding API

### 1. Start dependencies

```bash
docker run -d --name mysql -e MYSQL_ROOT_PASSWORD=password -e MYSQL_DATABASE=veda -p 3306:3306 mysql:8
docker run -d --name milvus -p 19530:19530 milvusdb/milvus:latest standalone
```

### 2. Configure

```bash
cp veda-server.toml.example veda-server.toml
```

```toml
[server]
listen_addr = "0.0.0.0:9009"
jwt_secret = "change-me"

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

Override via environment variables with `VEDA_` prefix.

### 3. Build and run

```bash
cargo build --release
./target/release/veda-server
```

### 4. Create account and workspace

```bash
# Register account, get API key
veda account create --email joe@example.com

# Create a workspace
veda workspace create my-project

# Connect CLI to workspace
veda use my-project
```

## Usage

### File Operations

```bash
# Upload files
veda cp ./README.md :/docs/readme.md
veda cp ./report.pdf :/docs/report.pdf    # extracts text for search

# Browse
veda ls :/docs
veda cat :/docs/readme.md
veda cat :/docs/readme.md --lines 10:20   # line-based reading

# Organize
veda cp :/docs/readme.md :/backup/readme.md
veda mv :/docs/old.md :/archive/old.md
veda rm :/tmp -r
```

### Search

```bash
# Hybrid search (semantic + BM25, default)
veda search "how does authentication work"

# Semantic only
veda search "error handling patterns" --mode semantic

# Full-text only
veda search "TODO fix" --mode fulltext
```

### Structured Collections

```bash
# Create a collection with schema
veda collection create articles \
  --field "title:string:index" \
  --field "content:string:embed" \
  --field "category:string:index"

# Insert rows (auto-embeds the content field)
veda collection insert articles \
  --data '{"title":"Intro to Rust","content":"Rust is a systems...","category":"tech"}'

# Search with filters
veda collection search articles "systems programming" \
  --filter "category == 'tech'" --limit 10

# SQL query
veda sql "SELECT title, category FROM articles WHERE category = 'tech' LIMIT 5"
```

### Raw Vector Collections

```bash
veda collection create my-vectors --dim 768 --type raw
veda insert --vector "[0.1, 0.2, ...]" --payload '{"label":"example"}'
veda collection search my-vectors --vector "[0.1, 0.2, ...]"
```

## Project Structure

```
veda/
├── crates/
│   ├── veda-types/      # Domain types, error definitions (zero dep)
│   ├── veda-core/       # Traits + business logic (no storage impl)
│   ├── veda-store/      # MySQL + Milvus implementations
│   ├── veda-pipeline/   # Embedding, chunking, PDF/OCR extraction
│   ├── veda-sql/        # DataFusion SQL engine
│   ├── veda-server/     # Axum HTTP server (thin shell)
│   ├── veda-cli/        # CLI client
│   └── veda-fuse/       # FUSE mount (optional, excluded from default build)
├── docs/
│   ├── design.md        # Complete design document
│   └── PLANS.md         # Sprint planning
└── AGENTS.md
```

## Search Modes

| Mode | How it works | Best for |
|------|-------------|----------|
| **hybrid** (default) | Vector + BM25, fused with RRF | General purpose |
| **semantic** | Cosine similarity | Conceptual search |
| **fulltext** | BM25 keyword | Exact terms, identifiers |

## License

MIT
