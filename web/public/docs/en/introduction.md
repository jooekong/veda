# What is Veda

**Veda** is a programmable knowledge store that unifies files, vector search, and SQL queries behind one API. One CLI, one HTTP interface; underneath it's MySQL (control plane) + Milvus (data plane) + an auto-embedding worker.

Think of it as **"a network drive that knows how to search itself"** plus **"a vector database that indexes itself"** plus **"a SQL engine that runs over both"**.

## Three ways to use it

Same data, three surfaces — pick by scenario:

- **CLI** — the `veda` binary, for scripts and everyday shell
- **FUSE mount** — `veda-fuse` mounts a workspace as a local directory. **vim / VSCode / `make` / `rsync` don't know it's cloud storage** — every write auto-uploads and re-embeds
- **HTTP API** — REST + SSE, OpenAPI-style JSON. Direct integration for frontends, custom agents, data pipelines; official SDKs (Python / TypeScript / Go) coming soon to make integration even cheaper

FUSE is particularly good for "I want my familiar editor but with semantic search on top".

---

## Core capabilities

| Capability | What it does |
|---|---|
| **File operations** | `cp` / `cat` / `ls` / `mv` / `rm` / `mkdir` / `append` — same semantics as Unix |
| **Hybrid search** | Every file is auto-chunked, embedded, indexed. Default `hybrid` (vector + BM25 + RRF); also `semantic` / `fulltext` alone |
| **Structured collections** | Vector-native database: define a schema + auto-embedded field, filter & search by other fields |
| **SQL queries** | DataFusion engine over files and collections — filter, aggregate, join |
| **Multi-tenant** | Account → Workspace; API key or workspace key auth |
| **FUSE mount** | Mount a workspace as a local directory; use vim / IDE / `make` like any native tree |
| **Layered summaries** | Auto-generated L0 (one-sentence) and L1 (~2k-token) summaries — saves tokens for LLM recall |

---

## Three layers per file *and* per directory

Veda auto-maintains two summaries plus the raw content for **every file and every directory** in your workspace, in increasing token cost:

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

- **Token cost scales with depth, not up front**: 100 L0s ≈ 5k tokens; 100 L1s ≈ 200k; 100 full files ≈ MB-scale. Escalate L0 → L1 → full on demand instead of paying all-in.
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

Notes, technical docs, paper highlights, code snippets — all go in:

```bash
veda cp ~/Notes/2026-blockchain-paper.md /papers/blockchain-2026.md
veda cp -r ~/Notes/work /notes/work
veda search "how does raft handle leader change"
```

Unlike a traditional doc store: search isn't grep-by-keyword, it's semantic recall. A note from six months ago that mentioned "distributed consensus" will surface when you search "raft" today.

### 2. AI agent memory + distributed state

This is Veda's **flagship use case**. LLMs forget across sessions; multi-agent / multi-host setups also face "how do we share context and intermediate state". One workspace + auto-embedding + cross-host reads/writes solves all of these.

**a. Cross-session long-term memory**

```bash
# Agent dumps the session summary
veda cp /tmp/session-2026-05-19.md /conversations/2026-05-19.md
# Next session opens with a three-layer escalation
veda search "the deployment plan we discussed" --detail-level abstract  # L0: relevance
veda overview /conversations/2026-05-19.md                              # L1: structured
veda cat /conversations/2026-05-19.md                                    # full when needed
```

The L0 → L1 → full ladder lets the model trade token budget against detail depth on its own.

**b. Cross-host / cross-instance distributed state**

One agent on multiple machines, or several agent instances in parallel — each gets a `wk_` and shares the same workspace; state syncs automatically:

```bash
# Instance A on host 1: writes the latest todo
veda cp /tmp/todo.json /state/agent-todo.json

# Instance B on host 2: reads it (push via SSE or poll)
veda cat /state/agent-todo.json
```

Veda's server emits server-sent events; file changes propagate to other clients within ~120s without polling.

**c. Checkpoint & resume for long jobs**

For jobs that span hours or days, write a checkpoint per step:

```bash
veda cp /tmp/step-12-result.json /checkpoints/job-X/step-12.json
# After crash / reboot
veda ls /checkpoints/job-X         # finds step-12, picks up at step-13
```

**d. Multi-agent collaboration**

Different roles (planner / coder / reviewer) each write outputs into subdirs; downstream agents read by search or fixed path:

```bash
# planner writes
veda cp plan.md /agents/planner/2026-05-19-plan.md
# coder finds the latest plan
veda search "deployment plan" --path /agents/planner --limit 1
```

**e. Pre-warmed knowledge bases (RAG)**

Embed your repo / docs / papers once; agents recall with zero latency:

```bash
veda cp -r ~/work/internal-docs /knowledge/internal
veda cp -r ~/work/code-A /knowledge/code-A
# Agent at runtime
veda search "how is our retry policy defined" --detail-level abstract
```

The bundled [skill system](#/docs/skill) teaches Claude Code, Codex, Cursor, etc. to call the `veda` CLI correctly without you writing prompts.

### 3. Search across multiple repos

Mirror selected subtrees from many team repos into one workspace:

```bash
veda cp -r ~/work/repo-a /code/repo-a
veda cp -r ~/work/repo-b /code/repo-b
veda search "how is retry handled" --path /code
veda grep "TODO(joe)" --limit 200      # literal grep also works, with line numbers
```

`veda grep` is synchronous literal match (no embedding lag) — good for specific identifiers. `veda search` is async semantic recall — good for concepts.

### 4. Structured data with vectors

Filtered RAG:

```bash
veda collection create articles \
  --schema '[{"name":"title","type":"string","index":true},
             {"name":"content","type":"string"},
             {"name":"category","type":"string","index":true}]' \
  --embed-source content

veda collection insert articles '[{"title":"Intro to Rust","content":"...","category":"tech"}]'

veda collection search articles "systems programming" --limit 5
veda sql "SELECT title FROM articles WHERE category='tech' LIMIT 5"
```

The `content` field is auto-embedded; `title` / `category` are filter indexes. More structure than a pure vector store, more semantics than a pure document DB.

### 5. Shared team context

A single workspace can have many keys handed out to people or agents:

- Teammates each get a `wk_readwrite`, all writing into the same workspace
- CI/CD gets a `wk_read`, read-only consumer
- Revoke any one key without touching the account

### 6. Mount the workspace as a directory

Skip the CLI, edit with vim / VSCode directly:

```bash
veda-fuse mount --server $URL --key $WK ~/veda
cd ~/veda
vim notes/today.md      # any editor
git status              # regular git works
```

Every write auto-uploads and re-embeds. SSE stream propagates changes to other clients within ~120s.

---

## What it's NOT good at

| Use case | Veda fit |
|---|---|
| Binary blobs (PDF / images / video) | ❌ UTF-8 text only |
| Strict ACLs / quotas | ❌ Fine-grained perms not in alpha |
| High-concurrency OLTP | ❌ It's a knowledge store, not a transactional DB |
| Massive small files (>1M chunks) | ⚠️ Alpha is single-replica; scale-out is on the roadmap |
| Documents + metadata | ✅ File body + collection schema together |
| Long-context LLM recall | ✅ Layered summaries + hybrid search are designed for this |

---

## Next

- [**Quickstart**](#/docs/quickstart) — 5 minutes from onboard to first search
- [**CLI reference**](#/docs/cli) — every command on one page
- [**AI agent skill**](#/docs/skill) — wire Veda into Claude Code / Cursor / Codex
- [**FUSE mount**](#/docs/fuse) — workspace as a local directory
