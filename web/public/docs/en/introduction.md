# What is Veda

**Veda** is a programmable knowledge store that unifies files, vector search, and SQL queries behind one API. One CLI, one HTTP interface; underneath it's MySQL (control plane) + Milvus (data plane) + an auto-embedding worker.

Think of it as **"a network drive that knows how to search itself"** plus **"a vector database that indexes itself"** plus **"a SQL engine that runs over both"**.

## Three ways to use it

Same data, three surfaces — pick by scenario:

- **CLI** — the `veda` binary, for scripts and everyday shell
- **FUSE mount** — `veda-fuse` mounts a workspace as a local directory. **vim / VSCode / `make` / `rsync` don't know it's cloud storage** — every write auto-uploads and re-embeds
- **HTTP API** — REST + SSE, OpenAPI-style JSON. Direct integration for frontends, custom agents, data pipelines; official SDKs (Python / TypeScript / Go) coming soon to lower integration effort further

---

## Core capabilities

| Capability | What it does |
|---|---|
| **File operations** | `cp` / `cat` / `ls` / `mv` / `rm` / `mkdir` / `append` — same semantics as Unix |
| **Hybrid search** | Every file is auto-chunked, embedded, indexed. Default `hybrid` (vector + BM25 + RRF); also `semantic` / `fulltext` alone |
| **Structured collections** | Like a vector-native database: define a schema + auto-embedded field, filter & search by other fields |
| **SQL queries** | DataFusion engine over files and collections — filter, aggregate, join |
| **Multi-tenant** | Account → Workspace; API key or workspace key auth |
| **FUSE mount** | Mount a workspace as a local directory; use vim / IDE / `make` like any native tree |
| **Layered summaries** | Auto-generated L0 (one-sentence) and L1 (~2k-token) summaries — saves tokens for LLM recall |

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
- **Computed once on the server, shared across clients**: CLI, FUSE, and custom agents all read the same precomputed summaries — no per-client recomputation, no two agents inventing inconsistent summaries.
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

## Typical use cases

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

The bundled [skill system](#/docs/skill) teaches Claude Code, Codex, Cursor, etc. to call the `veda` CLI correctly without you writing prompts.

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

- Teammates each get a `wk_readwrite`
- CI/CD gets a `wk_read`
- Revoke any one key without touching the account

### 6. Mount the workspace as a directory

`vim` / VSCode / `make` work directly on the remote workspace — see [FUSE mount](#/docs/fuse).

---

## What it's NOT good at

| Use case | Limit |
|---|---|
| Binary blobs (PDF / images / video) | ❌ UTF-8 text only |
| Strict ACLs / quotas | ❌ Fine-grained perms not in alpha |
| High-concurrency OLTP | ❌ It's a knowledge store, not a transactional DB |
| Massive small files (>1M chunks) | ⚠️ Alpha is single-replica; scale-out is on the roadmap |

---

## Next

- [**Quickstart**](#/docs/quickstart) — 5 minutes from onboard to first search
- [**CLI reference**](#/docs/cli) — every command on one page
- [**AI agent skill**](#/docs/skill) — wire Veda into Claude Code / Cursor / Codex
- [**FUSE mount**](#/docs/fuse) — workspace as a local directory
