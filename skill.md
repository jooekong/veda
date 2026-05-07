---
name: veda
description: |
  Knowledge store with file ops, semantic+fulltext search, structured collections,
  and SQL queries. Use when user mentions veda, wants to upload/cat/list/search
  files in their personal knowledge base, manage structured collections, run SQL
  across their data, or mount their workspace as a local filesystem (FUSE).
---

# Using Veda

Veda is a multi-tenant knowledge store: files + vector search + SQL, all through one CLI.
You interact with it via the `veda` binary; never construct HTTP requests directly.

## Bootstrap (first-time setup)

1. **Check the binary is installed**:
   ```sh
   which veda
   ```
   If missing, install:
   ```sh
   curl -fL https://raw.githubusercontent.com/jooekong/veda/main/install.sh | sh
   ```
   Add `--with-fuse` to also install `veda-fuse` for filesystem mounting.

2. **Configure server URL** (one-time):
   ```sh
   veda config set server_url http://10.79.51.161:3000
   ```

3. **Get an API key** — ask the user to run this themselves; do not type their password for them:
   ```sh
   veda account create --name "Their Name" --email user@example.com --password '<their-password>'
   ```
   The user runs it, copies the printed `api_key`, and pastes it back. Then:
   ```sh
   veda config set api_key <pasted-key>
   ```

4. **Create or pick a workspace**:
   ```sh
   veda workspace list
   veda workspace create my-project    # if none exist
   veda workspace use my-project       # writes workspace_id + workspace_key into config
   ```

5. **Verify**:
   ```sh
   veda ls /
   ```
   Empty listing or existing files is fine. Errors mean a step above failed.

## Path syntax

Remote paths are plain absolute paths starting with `/`, scoped to the active workspace.
The CLI **does not accept** `:/path` syntax — `:` is rejected as an invalid character.

## Core operations

### File operations

```sh
veda cp ./README.md /docs/readme.md         # upload
veda cp ./report.pdf /docs/report.pdf       # PDF — text auto-extracted
veda cp - /notes/scratch < some-input       # from stdin (use "-" as src)
veda ls /docs                                # list dir
veda cat /docs/readme.md                     # full content
veda cat /docs/readme.md --lines 1:20       # line range (1-indexed, inclusive)
veda mv /old-name /new-name                  # rename
veda rm /path                                # delete (recursive by default — no -r flag)
veda mkdir /new-dir                          # create directory
veda append /notes/log "new entry"          # append (use "-" for stdin)
```

### Search

Three modes; default `hybrid` fuses semantic + BM25 with RRF.

```sh
veda search "how does authentication work"                    # hybrid (best general)
veda search "fn parse_token" --mode fulltext                  # exact strings, identifiers
veda search "code about retry semantics" --mode semantic      # conceptual only
veda search "summary" --detail-level abstract                 # save tokens — return summaries
```

Flags: `--mode {hybrid,semantic,fulltext}`, `--limit N` (default 10),
`--detail-level {abstract,overview,full}` (default `full`).

**Embedding is async**: a file uploaded just now may not appear in semantic results
for a few seconds. If a search misses an obviously-present file, retry after 5s.

### Grep (substring match)

Use this when the user wants `grep`-style: literal substring, exhaustive, with
file paths and line numbers — not BM25 ranking. No embedding lag (synchronous).

```sh
veda grep "TODO(joe)"                      # scan whole workspace
veda grep "panic!" /code                   # scope to a path prefix
veda grep "Outbox" -i                      # case-insensitive
veda grep "fn main" --limit 200            # raise the cap (default 100, max 1000)
```

Output: `path:line_no: line` per hit.

`veda grep` vs `veda search --mode fulltext`: fulltext is **BM25 ranking** (best for
"find the most relevant chunks containing these terms"). Grep is **literal substring
across all files** (best for symbols, identifiers, exact strings, exhaustive search).

### Summary

```sh
veda summary /path/to/file.md       # L0/L1 auto-generated summary for a file
veda summary /                       # workspace-level summary
```

L0/L1 summaries are generated **asynchronously**. If you ask for a summary right
after uploading, the CLI prints `Summary not ready yet (summary pending)` and exits
with code 2 (server returns HTTP 202 Accepted with `Retry-After: 5`). Wait ~5 seconds
and retry, or fall back to `veda cat` for the raw content. If the path itself
doesn't exist you still get `404`.

### Structured collections

Schema-first, vector-native.

```sh
# Create with schema (JSON array). --embed-source picks which field is auto-embedded.
veda collection create articles \
  --schema '[
    {"name":"title","type":"string","index":true},
    {"name":"content","type":"string"},
    {"name":"category","type":"string","index":true}
  ]' \
  --embed-source content

# Insert: positional argument, JSON ARRAY of rows (not single object)
veda collection insert articles '[
  {"title":"Intro to Rust","content":"Rust is a systems language...","category":"tech"},
  {"title":"Pasta Recipe","content":"Boil water...","category":"food"}
]'

# List / describe / delete
veda collection list
veda collection desc articles
veda collection delete articles

# Search by semantic similarity (no built-in --filter; use SQL for filtering)
veda collection search articles "systems programming" --limit 5

# Raw vector collection: declare a vector field in the schema
veda collection create my-vecs \
  --schema '[{"name":"v","type":"vector","dim":768},{"name":"label","type":"string"}]' \
  --embed-source v
```

**Important**: `veda collection search` output dumps the full embedding vector for
every result (1024+ floats per row, ~30KB each). When parsing this in an LLM
context, **always strip the `vector` field** before processing — only keep
`title`, `content`, `category` (etc.) and `distance`. Or use `veda sql` instead
for cleaner output.

### SQL queries

DataFusion engine. Cleaner output than `collection search` (no vector dump).
Use this when you need filters or aggregations on a collection.

```sh
veda sql "SELECT title, category FROM articles WHERE category = 'tech' LIMIT 5"
veda sql "SELECT category, COUNT(*) FROM articles GROUP BY category"
```

Output is JSON-line (one row per line).

### FUSE mount (only if installed with --with-fuse)

```sh
veda-fuse mount /mnt/veda --server $SERVER --key $WORKSPACE_KEY
# now use ripgrep, ls, $EDITOR, etc. on /mnt/veda
veda-fuse umount /mnt/veda
```

## Decision rules — pick the right command

| User wants                             | Use                                      |
|----------------------------------------|------------------------------------------|
| Upload a local file                    | `veda cp local /remote`                  |
| Find old notes by topic                | `veda search` (default hybrid)           |
| Find exact identifier / string (ranked)| `veda search --mode fulltext`            |
| Find every literal occurrence + line # | `veda grep` (substring, exhaustive)      |
| Recall conceptually similar content    | `veda search --mode semantic`            |
| Save tokens on long results            | `veda search --detail-level abstract`    |
| Tabular data with schema               | Collection (not file)                    |
| Filter / aggregate over a collection   | `veda sql` (collection search has no filter) |
| Aggregation across many files          | `veda sql`                               |
| Edit / grep many files locally         | FUSE mount + native tools                |
| List / browse files                    | `veda ls`                                |
| Read specific file                     | `veda cat`                               |

## Error handling

| Error message                          | Meaning              | Fix                                              |
|----------------------------------------|---------------------|--------------------------------------------------|
| `invalid path: invalid character in segment: :` | Used `:/path` syntax | Drop the `:` — paths are plain `/path`     |
| `no API key configured`                | Not authenticated    | Ask user to run `veda account create` or `login` |
| `no workspace selected`                | No active workspace  | `veda workspace list` → `veda workspace use <name>` |
| `connection refused` / `Empty reply from server` | Server unreachable / hung | `veda config show`; verify server URL, ping it |
| `401 unauthorized`                     | API key invalid      | `veda account login` to refresh                  |
| `Summary not ready yet (summary pending)` (exit 2) | L0/L1 generation is async | Wait ~5s and retry, or `veda cat` for raw content |
| 4xx with JSON error body               | API error            | Echo server's `error` field to user              |

## Don't do

- Don't store large binaries — PDFs/images keep extracted text only; the original is dropped.
- Don't use `veda search` for exact path lookups — use `ls` / `cat` instead.
- Don't write to `/` root — pick a semantic prefix like `/notes/`, `/code/`, `/docs/`.
- Don't repeat the user's password / API key in chat — they live in
  `~/.config/veda/config.toml` (mode 0600) once configured.
- Don't construct HTTP requests directly — always go through the `veda` CLI.
- Don't display raw `veda collection search` output to the user — strip the
  `vector` field first or use `veda sql` instead.

## Reference

- Repo: https://github.com/jooekong/veda
- Server (alpha): http://10.79.51.161:3000
- Issues: contact Joe
