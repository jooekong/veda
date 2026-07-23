# What is Veda

**Veda** is a programmable knowledge store that unifies files, vector search, and SQL queries behind one API. One CLI, one HTTP interface; underneath it's MySQL (control plane) + Milvus (data plane) + an auto-embedding worker.

Think of it as **"a network drive that knows how to search itself"** plus **"a vector database that indexes itself"** plus **"a SQL engine that runs over both"**.

## Two workspace types

A Veda workspace is one of two kinds, fixed at creation — pick by scenario:

| | File Workspace | Vector Workspace |
|---|---|---|
| **kind** | `fs` | `db` |
| **Data model** | files / directories | vector records (text + meta) |
| **Access** | CLI / FUSE / HTTP, `wk_` | REST API / SDK, data-plane `wk_` (control-plane `vk_`) |
| **Typical use** | personal knowledge base, agent memory, code search | managed vector retrieval for apps (Pinecone-style) |
| **Analogy** | a network drive that searches itself | a managed vector database |

Below, the **File Workspace comes first** (file ops, hybrid search, structured collections, SQL, FUSE, tiered summaries), **then the Vector Workspace** (managed embedding, hybrid retrieval, meta filtering). If you came here to add vector search to an application, jump straight to "Vector Workspace: capabilities & use cases".

## Four ways to use it

Same data, four surfaces — pick by scenario:

- **CLI** — the `veda` binary, for scripts and everyday shell
- **FUSE mount** — `veda-fuse` mounts a workspace as a local directory. **vim / VSCode / `make` / `rsync` don't know it's cloud storage** — every write auto-uploads and re-embeds
- **MCP** — the server-native `/mcp` endpoint: Claude Code / Cursor and other coding agents attach with one `.mcp.json` entry (URL + `wk_` bearer), zero install (see [AI agent skill](#/docs/skill))
- **HTTP API** — REST + SSE JSON interface. Direct integration for frontends, custom agents, data pipelines; the Vector Workspace also ships a Java SDK and Python examples (see [Vector Workspace API](#/docs/vectors))

---

## File Workspace: core capabilities

| Capability | What it does |
|---|---|
| **File operations** | `cp` / `cat` / `ls` / `mv` / `rm` / `mkdir` / `append` — same semantics as Unix |
| **Hybrid search** | Every file is auto-chunked, embedded, indexed. Default `hybrid` (vector + BM25 + RRF); also `semantic` / `fulltext` alone |
| **Structured collections** | Like a vector-native database: define a schema + auto-embedded field, filter & search by other fields |
| **SQL queries** | DataFusion engine over files and collections — filter, aggregate, join |
| **Multi-tenant** | Two tiers: Account → Workspace; control-plane account key `vk_`, data-plane workspace key `wk_` |
| **FUSE mount** | Mount a workspace as a local directory; use vim / IDE / `make` like any native tree |
| **Layered summaries** | Auto-generated L0 (one-sentence) and L1 (~2k-token) summaries — saves tokens for LLM recall |
| **Agent access / RAG answering** | MCP endpoint with 6 read-only tools for coding agents; `/v1/answer` / `veda ask` return answers with inline [n] citations |

---

## Three layers per file *and* per directory

| Layer | Command | Size | For files | For directories |
|---|---|---|---|---|
| **abstract** (L0) | `veda abstract /path` | 1 sentence | one-sentence file summary | one-sentence summary of what's in the dir |
| **overview** (L1) | `veda overview /path` | ~2k tokens | structured prose (sections, claims, key data) | hierarchical summary of subdirs + files |
| **full** | `veda cat /path` or `veda ls /path` | full content / listing | raw text | listing |

```bash
veda abstract /docs/readme.md      # file L0
veda abstract /knowledge/auth      # directory L0 ← one sentence for a whole subtree
veda overview /knowledge/auth      # directory L1 ← structured tree summary
veda cat /docs/readme.md           # raw text
```

### Why this matters

- **Token cost scales with depth, not up front**: 100 L0s ≈ 5k tokens; 100 L1s ≈ 200k tokens; 100 full files ≈ MB-scale. Escalate L0 → L1 → full on demand instead of paying all-in.
- **Directory exploration is nearly free**: `veda abstract /knowledge/internal/auth` tells you what a subtree is "about" in one sentence — no `ls && cat` loop.
- **Computed once on the server, shared across clients**: CLI, FUSE, and custom agents all read the same precomputed summaries — no two agents inventing inconsistent summaries from different prompts.
- **The model picks the depth**: instead of a fixed top-k cutoff, agents look at L0 hits and decide per-result whether to expand to L1 or accept the one-sentence answer.

### In search and FUSE

`search` exposes the ladder directly:

```bash
veda search "deployment plan" --detail-level abstract     # results are L0 only
veda search "..." --detail-level overview                 # results include L1
veda search "..." --detail-level full                     # full content (default)
```

In FUSE mode, the summaries appear as sidecar files inside every mounted directory:

```bash
cat /mnt/veda/docs/.abstract       # current dir L0
cat /mnt/veda/docs/.overview       # current dir L1
```

> Summaries are produced on the same async worker pipeline as embeddings. A just-uploaded file or directory may return `Summary not ready yet` for a few seconds — retry shortly.
> The workspace root `/` has no summary (no `/` dentry on the server) — start from any subdirectory.

---

## File Workspace: typical use cases

### 1. Personal knowledge base

Notes, docs, paper highlights, code snippets — drop them in and recall by meaning (not grep):

```bash
veda cp ~/Notes/2026-blockchain-paper.md /papers/blockchain-2026.md
veda cp -r ~/Notes/work /notes/work
veda search "how does raft handle leader change"
```

### 2. AI agent memory + distributed state

**a. Cross-session long-term memory** — agent dumps a session summary; the next session can use the three-layer ladder to recall what's relevant:

```bash
veda cp /tmp/session-2026-05-19.md /conversations/2026-05-19.md
veda search "the deployment plan we discussed" --detail-level abstract
veda overview /conversations/2026-05-19.md     # escalate when needed
```

**b. Cross-host / cross-instance distributed state** — multiple agent instances share one workspace; SSE pushes file changes within ~120s:

```bash
veda cp /tmp/todo.json /state/agent-todo.json   # instance A writes
veda cat /state/agent-todo.json                 # instance B reads
```

**c. Checkpoint & resume for long jobs** — write a checkpoint per step; resume on crash:

```bash
veda cp /tmp/step-12-result.json /checkpoints/job-X/step-12.json
veda ls /checkpoints/job-X
```

**d. Multi-agent collaboration** — planner / coder / reviewer write into subdirs; downstream agents search for upstream outputs:

```bash
veda cp plan.md /agents/planner/2026-05-19-plan.md
veda search "deployment plan" --path /agents/planner --limit 1
```

**e. Pre-warmed knowledge bases (RAG)** — embed repos / docs upfront; agents recall with zero latency:

```bash
veda cp -r ~/work/internal-docs /knowledge/internal
veda search "how is our retry policy defined" --detail-level abstract
```

With the bundled [skill system](#/docs/skill), Claude Code / Codex / Cursor automatically learn to call the `veda` CLI.

### 3. Search across multiple repos

```bash
veda cp -r ~/work/repo-a /code/repo-a
veda cp -r ~/work/repo-b /code/repo-b
veda search "how is retry handled" --path /code
veda grep "TODO(joe)" --limit 200      # literal, sync, file:line
```

`grep` is synchronous literal match for identifiers; `search` is async semantic recall for concepts.

### 4. Structured data with vectors

Filtered RAG. Full schema + commands in [CLI reference — Structured collections](#/docs/cli); summary: the `content` field auto-embeds, `title` / `category` are filter indexes, use `veda sql` for filters / aggregates.

### 5. Shared team context

One workspace can hand out many keys:

- Teammates each get a `wk_readwrite` to write notes
- CI/CD gets a read-only `wk_read`
- Revoke any one key without touching the account

### 6. Mount the workspace as a directory

`vim` / VSCode / `make` work directly on the remote workspace — see [FUSE mount](#/docs/fuse).

---

## Vector Workspace: capabilities & use cases

If what you need is not "store files" but **add semantic retrieval to an application**, create a `kind=db` Vector Workspace. It's Pinecone-style managed vector retrieval, and the pitch fits in one sentence: **you write text, never vectors** — the embedding model, the vector index, and the BM25 full-text index all live server-side. Your app needs no embedding service and never cares about models or dimensions.

| Capability | What it does |
|---|---|
| **Managed embedding** | Writes carry only `text`; the server embeds and indexes it. Swapping the embedding model is a server concern — application code doesn't change |
| **Three retrieval modes** | `hybrid` (vector + BM25 + RRF fusion, default) / `semantic` (pure vector, supports a `min_score` relevance floor) / `fulltext` (pure BM25 — no embedding call, the cheapest) |
| **Meta filtering** | Each record carries `category` / `tags` / arbitrary `meta` JSON; search filters on `meta.<key>` with eq / in / range ops — "filtered RAG" out of the box |
| **Dataset grouping** | Group records inside one workspace (`products`, `faq`, …) without cross-talk; omitted means `default` |
| **Two write modes** | Default `upsert` is idempotent and retry-safe; bulk imports can use `insert` to skip dedup for ~3x throughput (caller guarantees id uniqueness) |
| **Multi-tenant isolation** | One workspace = one dedicated Milvus collection; data-plane `wk_` keys are issued per workspace with read / readwrite scopes — revoke one key without touching the others |
| **Access** | REST API (plain curl works) + Java SDK; the control plane (creating workspaces, minting keys) is handled by the platform / console — an app just receives its `wk_` and goes |

### The 30-second version

```bash
WK=wk_...   # data-plane key issued by the platform, bound to your workspace

# Write: just text + business fields; the server embeds
curl -sX POST $BASE/v1/vectors/upsert \
  -H "authorization: Bearer $WK" -H 'content-type: application/json' \
  -d '{"records":[
        {"id":"sku-1","text":"Air Jordan 1 retro basketball shoes","meta":{"price":1299}},
        {"id":"sku-2","text":"Stan Smith classic white sneakers","meta":{"price":499}}]}'

# Search: hybrid semantic + full-text recall, with a meta filter
curl -sX POST $BASE/v1/vectors/search \
  -H "authorization: Bearer $WK" -H 'content-type: application/json' \
  -d '{"query":"basketball shoes under 1500","top_k":5,
       "filter":{"must":[{"field":"meta.price","op":"lt","value":1500}]}}'
```

### Typical use cases

**1. Semantic search inside an application** — products, FAQ, tickets, content libraries. Queries that keyword search can't serve become semantic recall: for "basketball shoes under 1500", BM25 catches model names, the vector side catches intent, RRF fuses the ranking, and the meta filter enforces the price range.

**2. RAG knowledge backbone** — the application owns its chunking and content; whatever it writes is retrievable. `semantic` + `min_score` puts a relevance floor under recall so low-relevance content doesn't pollute the prompt; cost-sensitive paths can run `fulltext` (no embedding call).

**3. Similar content / recommendations** — "people also viewed", duplicate-ticket merging, near-duplicate detection: use the current item's text as the query in `semantic` mode — no separate similarity service.

**4. Isolating business lines** — one account, many workspaces (or one workspace, many datasets); each consumer gets its own `wk_`. When one integration is retired or a key leaks, revoke that one key — nothing else is affected.

### File Workspace or Vector Workspace?

- Data is **files / documents**, humans read and write it via CLI or editors, you want directories, summaries, SQL → **File Workspace**
- Data is **business records** (products, FAQ entries, content chunks), written and queried by an application, you want filtering and throughput → **Vector Workspace**
- Both? One account can hold any mix of the two kinds — they don't interact.

For the field-level contract (all endpoints, limits, error codes, idempotency semantics) see [Vector Workspace API](#/docs/vectors).

---

## What it's NOT good at / limits

| Use case | Limit |
|---|---|
| Images / video / scans | ❌ Not parsed (no OCR yet). In the File Workspace, PDF / Word get text-extracted and searchable; other binaries are stored but not indexed. The Vector Workspace still accepts text only |
| Strict ACLs / quotas | ❌ Fine-grained perms not in alpha |
| High-concurrency OLTP | ❌ It's a knowledge / retrieval store, not a transactional DB |
| Massive small files (>1M chunks) | ⚠️ Alpha is single-replica; scale requires the multi-replica evolution |
| Bring-your-own vectors | ❌ The Vector Workspace accepts text only (managed embedding) — no pre-computed vectors |
| Cross-dataset search | ❌ One search call targets one dataset; fan out client-side for more |
| Very long single records | ⚠️ Vector Workspace caps `text` at 64KB per record — chunk longer content client-side |

---

## Next

- [**Quickstart**](#/docs/quickstart) — 5 minutes from onboard to first search
- [**Full reference**](#/docs/reference) — architecture / auth / all APIs / error codes / limits, all on one page
- [**Vector Workspace API**](#/docs/vectors) — business-facing managed vector retrieval
- [**CLI reference**](#/docs/cli) — every command on one page
- [**AI assistant integration**](#/docs/skill) — wire Veda into Claude Code / Cursor / Codex
- [**FUSE mount**](#/docs/fuse) — workspace as a local directory
