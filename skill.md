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

Onboarding is now anonymous-first: two commands and you're usable.

1. **Check the binary is installed**:
   ```sh
   which veda
   ```
   If missing, install (the installer writes the default server URL into the
   config automatically; no extra `config set` step needed):
   ```sh
   curl -fL https://veda.dbpaas.dingdongxiaoqu.com/install.sh | sh
   ```
   Add `--with-fuse` to also install `veda-fuse` for filesystem mounting.

2. **Onboard**:
   ```sh
   veda init
   ```
   That's it. Zero prompts. The server mints an anonymous account + a default
   workspace + both keys in one round-trip, and `veda init` writes them all to
   `~/.config/veda/config.toml`. Run `veda status` to confirm.

3. **Verify**:
   ```sh
   veda ls /
   ```
   Empty listing is the expected first-run state. Errors mean step 1 or 2 failed.

### Upgrading anonymous to a named account

Anonymous identities live in `~/.config/veda/config.toml` only — useful for a
single machine, but you can't recover the account from another box. When the
user wants to attach an email/password (e.g. before moving to a new laptop):

```sh
veda claim --email you@example.com
```

`--password` is prompted (or read from `VEDA_PASSWORD`); the existing `vk_` key
keeps working, so no command line breaks.

### Existing user on a new machine

After `veda init` writes a fresh anon account on the new machine, you don't
want that — you want the claimed account. Two options:

```sh
# Option A: paste the api_key from the old machine
veda login --api-key vk_…   # account key → still need 'veda init' afterwards? no:
                            # use the equivalent below for a one-shot flow

# Option B: log in with email/password
rm ~/.config/veda/config.toml         # clear the anon state veda init wrote
veda config set server_url https://veda.dbpaas.dingdongxiaoqu.com
veda init --login --email you@example.com   # prompts password
```

## Path syntax

Remote paths are plain absolute paths starting with `/`, scoped to the active workspace.
The CLI **does not accept** `:/path` syntax — `:` is rejected as an invalid character.

## Setup

If `veda status` reports unconfigured (or any `veda <cmd>` errors with "no
API key configured"), the user hasn't onboarded yet. Single-command bootstrap:

```sh
veda init                                  # zero-prompt anonymous onboard (default)
veda init --email X --password Y           # register a named account (non-interactive)
veda init --login --email X --password Y   # attach an existing account silently
veda init --workspace dogfood              # named mode with custom workspace name
veda claim --email X                       # upgrade an existing anon account to named
```

`veda status` shows current config + server reachability. `veda login --api-key K`
swaps an existing `vk_*` (account) or `wk_*` (workspace) key without touching
the rest of the config.

## Core operations

### File operations

```sh
veda cp ./README.md /docs/readme.md         # upload (UTF-8 text only)
veda cp ./src /code -r                       # recursive directory upload
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

### Summary layers (abstract / overview)

Two layers, queried separately. **Default to `abstract` (L0)** — it's a single
sentence, cheap to fetch and usually all the agent needs to decide whether
the file is relevant. Only escalate to `overview` (L1, ~2k tokens of
structured prose) when the abstract is too thin to act on.

```sh
veda abstract /path/to/file.md     # L0 — one sentence, cheap
veda abstract /                     # workspace-level abstract
veda overview /path/to/file.md     # L1 — detailed prose, on demand
```

Both layers are generated **asynchronously**. If you ask right after uploading,
the CLI prints `Summary not ready yet (summary pending)` and exits with code
2 (server returns HTTP 202 Accepted with `Retry-After: 5`). Wait ~5 seconds
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

Server strips internal fields (`vector`, `workspace_id`) from collection
search output, so results are LLM-ready as-is. Prefer `veda sql` when you
need filters, aggregates, or tabular formatting.

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
| `no API key configured`                | Not authenticated    | Run `veda init` (anon) or `veda login --api-key <key>` |
| `no workspace selected`                | No active workspace  | Run `veda init` (anon onboard sets one) or `veda login --api-key wk_…` |
| `already onboarded`                    | `veda init` blocked from overwriting | Either `veda status` is what you want, or delete `~/.config/veda/config.toml` first |
| `connection refused` / `Empty reply from server` | Server unreachable / hung | `veda status` (checks reachability); verify `server_url` |
| `401 unauthorized`                     | API key invalid      | If you have email/password: `veda init --login --email …`; else paste a fresh key with `veda login --api-key`                 |
| `Summary not ready yet (summary pending)` (exit 2) | L0/L1 generation is async | Wait ~5s and retry, or `veda cat` for raw content |
| 4xx with JSON error body               | API error            | Echo server's `error` field to user              |

## Don't do

- Don't upload binaries — `veda cp` rejects non-UTF-8 input; PDFs/images
  aren't supported (no extraction backend wired up).
- Don't use `veda search` for exact path lookups — use `ls` / `cat` instead.
- Don't write to `/` root — pick a semantic prefix like `/notes/`, `/code/`, `/docs/`.
- Don't repeat the user's password / API key in chat — they live in
  `~/.config/veda/config.toml` (mode 0600) once configured.
- Don't construct HTTP requests directly — always go through the `veda` CLI.

## Reference

- Repo: https://github.com/jooekong/veda
- Server (alpha): https://veda.dbpaas.dingdongxiaoqu.com
- Issues: contact Joe
