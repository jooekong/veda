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
veda init --upgrade --email you@example.com
```

`--password` is prompted (or read from `VEDA_PASSWORD`); the existing `vk_` key
keeps working, so no command line breaks.

### Existing user on a new machine

Paste the key copied from the old machine. Works for both `vk_…` (account)
and `wk_…` (workspace) keys — prefix is auto-detected. Existing
`config.toml` (if any) is renamed to `config.toml.bak.<unix-ts>` before
writing, so you can always roll back:

```sh
veda init --import-key vk_…       # account key → also mints a fresh wk_ for "default"
veda init --import-key wk_…       # workspace key → data commands work immediately
```

No additional `veda workspace add` step is needed; `vk_` import auto-mints
a `default` profile in the same flow.

## Path syntax

Remote paths are plain absolute paths starting with `/`, scoped to the active workspace.
The CLI **does not accept** `:/path` syntax — `:` is rejected as an invalid character.

## Auth & workspace reference

All identity flows go through `veda init`, selected by mutually-exclusive mode
flags:

```sh
veda init                                       # zero-prompt anonymous onboard (default)
veda init --email X --password Y                # register a named account
veda init --login --email X --password Y        # attach an existing account
veda init --upgrade --email X                   # upgrade current anon to named (--password prompted)
veda init --import-key vk_…                     # paste a key copied from another machine
veda init --workspace-name dogfood              # named mode with custom workspace name
veda init --non-interactive --email X --password Y  # CI/script mode: fail instead of prompt
```

The `--login` / `--upgrade` / `--import-key` flags are mutually exclusive; clap
rejects any combination up front. `--import-key` accepts both `vk_*` (account)
and `wk_*` (workspace) keys; prefix decides the slot and existing config is
moved to `config.toml.bak.<unix-ts>` before overwrite.

### Multiple workspace profiles

`veda init` creates and selects a profile named `default`. Add more only when
you need to juggle several workspaces from one machine:

```sh
veda workspace add scratch                  # new server workspace named "scratch"
veda workspace add shared --workspace-id W  # mint a key for an existing workspace
veda workspace list                         # ★ marks the active profile
veda workspace switch scratch               # change the active profile (persisted)
veda workspace rm scratch                   # alias-only local delete (server key not revoked)

veda --workspace scratch ls /               # one-off override, active unchanged
```

**For agents / non-interactive scripts**: always pass `--workspace <alias>`
explicitly when the script depends on which workspace it lands in. The active
profile is shell-global state, so a stale `switch` from a previous session
would otherwise silently redirect your writes. `veda rm /path` blocks for
y/N confirmation on a TTY; in non-interactive mode it skips the prompt but
prints the target workspace alias to stderr so the caller can audit logs.

## Core operations

### File operations

```sh
veda cp ./README.md /docs/readme.md         # upload (UTF-8 text only)
veda cp ./src /code                          # directory upload — recursion is automatic on a dir src
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
veda overview /path/to/file.md     # L1 — detailed prose, on demand
```

(Workspace-root summaries `veda abstract /` / `veda overview /` aren't
implemented yet — the server has no dentry for `/`, so the request 404s.
Use a top-level subdirectory instead, e.g. `veda abstract /notes`.)

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

macOS needs `brew install macfuse` first. Linux uses libfuse3 (usually preinstalled).
GitLab release stream doesn't ship `veda-fuse` for `aarch64-apple-darwin` (cross-link
blocker, see `install.sh`); Apple Silicon users build from source or use the CLI only.

```sh
veda-fuse mount --server $SERVER --key $WORKSPACE_KEY /mnt/veda
# → prints "veda: daemon log → ~/.cache/veda-fuse/daemon.log"
# → prints "veda: mounted (pid 12345)" and returns immediately
veda-fuse umount /mnt/veda
```

Defaults to **daemon mode**: the command forks, detaches stdio, and the parent
returns as soon as the FUSE mount handshake completes. Pass `--foreground` to
keep the process attached to the terminal (logs go to stdout/stderr instead of
the daemon log file).

Mount flags / env vars:
- `--server <URL>` / `VEDA_SERVER` — server URL (required)
- `--key <KEY>` / `VEDA_KEY` — workspace key `wk_…` (required)
- `--foreground` — block in terminal, no daemon log
- `--cache-size <MB>` — read cache size (default 128)
- `--attr-ttl <SEC>` — attr cache TTL (default 5)
- `--allow-other` — share the mount with other UIDs (needs root in some envs)
- `--read-only` — RO mount
- `--debug` — verbose FUSE-layer logging
- `--write-mode {sync,writeback}` / `VEDA_FUSE_WRITE_MODE` — `sync` (default) writes through every flush; `writeback` buffers writes in an in-memory shadow and debounces commits, so vim swap / git lockfile / IDE temp files never reach the server.
- `--write-debounce-ms <N>` / `VEDA_FUSE_WRITE_DEBOUNCE_MS` — writeback debounce window (default 5000). Same-path writes inside the window coalesce to a single PUT.

**Writeback caveats**: in-memory only (a crash within the debounce window loses pending bytes — single-user alpha trade-off); per-file cap 10 MB / total cap 50 MB. Files that grow past the per-file cap silently degrade to synchronous writes for that file. `unmount` drains pending commits before exit so a normal shutdown loses nothing.

Diagnostics on failure: the parent prints either the preflight error directly
(e.g. `server health check failed (stat '/' at …)`) or `daemon exited before
mount was ready (exit N; check ~/.cache/veda-fuse/daemon.log for tracing
output)`. The daemon log captures the full startup trace — SSE connection,
FUSE init, any panic — in append mode.

#### Summary sidecars (`.abstract` / `.overview`)

When the server has summary capability enabled (the default), every mounted
directory exposes two read-only sidecar files:

```sh
cat /mnt/veda/docs/.abstract     # L0 one-sentence summary
cat /mnt/veda/docs/.overview     # L1 ~2k-token detailed summary
ls -a /mnt/veda/docs             # includes .abstract / .overview
```

Sidecars are server-generated and read-only (EROFS on write). Each acts like a
regular file once `cat` returns — `head`, `wc`, etc. work normally. Not-yet-ready
summaries return `summary pending; retry after a few seconds`. Servers built
without summary support return HTTP 501 and the sidecars are hidden (ENOENT).

`.abstract` / `.overview` directly at `/mnt/veda/` (workspace root) are not
implemented — use a subdirectory. If the workspace pre-dates the reserved-name
guard and contains real files named `.abstract` or `.overview`, the FUSE
sidecar shadows them on read; the real file is still reachable via `veda cat` /
`veda mv` to rename it out of the way.

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

### CLI

| Error message                          | Meaning              | Fix                                              |
|----------------------------------------|---------------------|--------------------------------------------------|
| `invalid path: invalid character in segment: :` | Used `:/path` syntax | Drop the `:` — paths are plain `/path`     |
| `no API key configured`                | Not authenticated    | Run `veda init` (anon) or `veda init --import-key vk_…` |
| `no workspace selected`                | No active workspace  | Run `veda init` (anon onboard sets one) or `veda init --import-key wk_…` |
| `already onboarded`                    | `veda init` blocked from overwriting | Run `veda init --import-key <key>` to swap identity (auto-backs up old config), or delete `~/.config/veda/config.toml` first |
| `connection refused` / `Empty reply from server` | Server unreachable / hung | `veda status` (checks reachability); verify `server_url` |
| `401 unauthorized`                     | API key invalid      | If you have email/password: `veda init --login --email …`; else paste a fresh key with `veda init --import-key`                 |
| `Summary not ready yet (summary pending)` (exit 2) | L0/L1 generation is async | Wait ~5s and retry, or `veda cat` for raw content |
| `Summary unavailable on this server` (exit 3) | Server returned HTTP 501 — summary feature disabled | Use `veda cat` for raw content; abstract/overview won't work against this server |
| 4xx with JSON error body               | API error            | Echo server's `error` field to user              |

### FUSE (veda-fuse mount)

| Symptom                                | Meaning              | Fix                                              |
|----------------------------------------|---------------------|--------------------------------------------------|
| `server health check failed (stat '/' at …)` (exit 1, immediate) | Preflight failed in daemon child — wrong URL, unreachable server, or bad key | Verify `--server` / `--key`; child writes this error through the readiness pipe so the parent prints it verbatim |
| `daemon exited before mount was ready (exit N; check …)` | Daemon child crashed after preflight, before FUSE init | Read `~/.cache/veda-fuse/daemon.log` — full tracing output is there. `killed by signal SIGILL` indicates a fork/runtime corruption (file a bug) |
| Mountpoint hangs / `Transport endpoint is not connected` | Daemon process died after a successful mount | `pgrep -af veda-fuse` to confirm; if no process, run `diskutil unmount force <path>` (macOS) or `fusermount -u <path>` (Linux) and re-mount |
| `.abstract` / `.overview` return ENOENT in a mounted dir | Server is built without summary capability (HTTP 501) | Expected — sidecars are hidden when server doesn't support them |

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
