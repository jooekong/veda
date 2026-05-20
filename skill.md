---
name: veda
description: |
  Knowledge store with file ops, semantic+fulltext search, structured collections,
  and SQL queries. Use when user mentions veda, wants to upload/cat/list/search
  files in their personal knowledge base, manage structured collections, run SQL
  across their data, or mount their workspace as a local filesystem (FUSE).
---

# Using Veda

Veda is a multi-tenant knowledge store: files + vector search + SQL, all through
one CLI. Interact via the `veda` binary; never construct HTTP requests directly.

## Quick start

```sh
which veda || curl -fL https://veda.dbpaas.dingdongxiaoqu.com/install.sh | sh
veda init       # zero-prompt anonymous onboard; writes ~/.config/veda/config.toml
veda ls /       # empty listing on first run is expected
```

Add `--with-fuse` to the installer for the FUSE mount client (see the
"FUSE mount" section below).

## Auth — one entry point, five modes

`veda init` is the only auth command. Modes are mutually exclusive:

```sh
veda init                                  # anonymous (default; no flags)
veda init --email X                        # register a named account
veda init --login --email X                # attach existing account
veda init --upgrade --email X              # turn current anon into named
veda init --import-key vk_…|wk_…           # paste a key from another machine
veda init --non-interactive --email X --password Y   # CI/script mode
```

`--import-key` auto-detects `vk_` (account, also mints a default `wk_`) vs
`wk_` (workspace, ready immediately). Existing `~/.config/veda/config.toml`
is moved to `config.toml.bak.<unix-ts>` before overwrite — rollback by
renaming back.

`--password` is read from `VEDA_PASSWORD` env or prompted; never put it
on argv (`ps` leak). All five modes accept `--non-interactive` to fail
instead of prompting.

### Workspace profiles

`veda init` creates one profile named `default`. Add more only when juggling
several workspaces from the same machine:

```sh
veda ws add scratch                  # short alias for `veda workspace add`
veda ws add shared --workspace-id W  # mint a key for an existing workspace
veda ws list                         # ★ marks the active profile
veda ws switch scratch               # change active profile (persisted)
veda ws rm scratch                   # local alias delete (server key not revoked)

veda --workspace scratch ls /        # one-off override, active unchanged
```

**For agents / scripts**: always pass `--workspace <alias>` when the script
depends on which workspace it lands in — the active profile is shell-global
state and a stale `switch` from another session can silently redirect writes.
`veda rm` blocks for y/N on a TTY; non-interactive skips the prompt but
prints the target workspace alias to stderr for audit.

## Path syntax

Remote paths are plain absolute paths starting with `/`, scoped to the
active workspace. The CLI **does not accept** `:/path` syntax — `:` is
rejected as an invalid character.

## File operations

```sh
veda cp ./README.md /docs/readme.md      # upload (UTF-8 text only)
veda cp ./src /code                       # directory — recursion auto on dir src
veda cp - /notes/scratch < input.txt     # from stdin (use "-" as src)
veda ls /docs                             # list dir
veda ls /docs --json                      # one JSON object per line (jq-friendly)
veda cat /docs/readme.md                  # full content
veda cat /file --range 1:20               # 1-indexed inclusive
veda cat /file --head 10                  # first 10 lines
veda cat /file --tail 5                   # last 5 lines (fetches full + slices)
veda mv /old /new                         # rename
veda rm /path                             # delete (recursive by default — no -r)
veda mkdir /new-dir                       # create directory
veda append /notes/log "entry"            # append (use "-" for stdin)
```

`veda cp` rejects non-UTF-8 input client-side with a path-aware error
(NUL-byte sniff + UTF-8 validation, before any HTTP call).

## Search

Three modes; default `hybrid` fuses semantic + BM25 with RRF:

```sh
veda search "how does authentication work"             # hybrid (general)
veda search "fn parse_token" --mode fulltext           # BM25 ranking
veda search "code about retry semantics" --mode semantic
veda search "anything" --detail-level abstract         # return L0 summaries
veda search "auth" --path /docs                        # restrict to subtree
```

Flags: `--mode {hybrid,semantic,fulltext}`, `--limit N` (10),
`--detail-level {abstract,overview,full}` (full), `--path /prefix`
(absolute, defaults to whole workspace). Add `--json` (global) for
one JSON hit per line. `--path` is server-side filtering — strictly
cheaper than client-side post-filter.

**Embedding is async**: a just-uploaded file may not appear in semantic
results for a few seconds. Retry after 5s if a search misses an obviously
present file.

## Grep (literal substring)

Synchronous, no embedding lag, exhaustive within `max_results`:

```sh
veda grep "TODO(joe)"                # whole workspace
veda grep "panic!" /code             # scope to path prefix
veda grep "Outbox" -i                # case-insensitive
veda grep "fn main" --limit 200      # raise cap (default 100, max 1000)
```

Output: `path:line_no: line` per hit. Use grep when you want every literal
occurrence with file:line; use `search --mode fulltext` when you want BM25
ranking on the same string.

## Summary layers (abstract / overview)

Two layers, queried separately. **Default to `abstract` (L0)** — one
sentence, cheap, usually enough to decide whether the file is relevant.
Escalate to `overview` (L1, ~2k tokens) only when needed.

```sh
veda abstract /path/to/file.md       # L0 — one sentence
veda overview /path/to/file.md       # L1 — structured prose
```

Both generated asynchronously. On a not-yet-ready path the CLI prints
`Summary not ready yet` and exits 2 (server sends HTTP 202 + `Retry-After`);
wait ~5s and retry, or fall back to `veda cat`. Server built without
summary support returns 501 and the CLI exits 3.

Workspace-root summaries (`veda abstract /`) aren't implemented — the
server has no `/` dentry. Use a subdirectory.

## Structured collections

Schema-first, vector-native:

```sh
# Create — JSON-array schema, --embed-source picks the auto-embedded field.
veda collection create articles \
  --schema '[{"name":"title","type":"string","index":true},
             {"name":"content","type":"string"},
             {"name":"category","type":"string","index":true}]' \
  --embed-source content

# Insert — JSON ARRAY of rows (not a single object).
veda collection insert articles '[
  {"title":"Intro to Rust","content":"…","category":"tech"},
  {"title":"Pasta","content":"…","category":"food"}
]'

veda collection list | desc <name> | delete <name>
veda collection search articles "systems programming" --limit 5
```

Raw vector collection: include `{"name":"v","type":"vector","dim":768}` in
the schema and `--embed-source v`.

Server strips internal fields (`vector`, `workspace_id`) from search
output, so results are LLM-ready. Prefer `veda sql` when you need filters,
aggregates, or tabular formatting.

## SQL queries

DataFusion engine. Cleaner than `collection search` (no vector dump).
Use when you need filters or aggregations:

```sh
veda sql "SELECT title, category FROM articles WHERE category='tech' LIMIT 5"
veda sql "SELECT category, COUNT(*) FROM articles GROUP BY category"
```

Output: one JSON row per line.

## FUSE mount (only if installed with `--with-fuse`)

`veda-fuse` mounts a Veda workspace as a local FUSE filesystem so native
tools (vim, IDE, `rsync`, `make`, `ls -l`) can read and write workspace
files like a normal directory tree. macOS needs `brew install --cask
macfuse`; Linux uses libfuse3 (the installer auto-installs it on
Debian / RHEL family).

```sh
veda-fuse mount --server $SERVER --key $WORKSPACE_KEY /mnt/veda
# → prints "veda: daemon log → ~/.cache/veda-fuse/daemon.log"
# → prints "veda: mounted (pid 12345)" and returns immediately
veda-fuse umount /mnt/veda
```

Default is **daemon mode**: the command forks, detaches stdio, and the
parent returns as soon as the FUSE handshake completes. Pass
`--foreground` to keep it attached (logs go to stdout/stderr instead
of the daemon log file). GitLab release stream doesn't ship `veda-fuse`
for `aarch64-apple-darwin` (cross-link blocker); Apple Silicon users
build from source or use the CLI only.

### Mount flags / env vars

| Flag / env                                                | Effect                                                                  |
|-----------------------------------------------------------|-------------------------------------------------------------------------|
| `--server <URL>` / `VEDA_SERVER`                          | Server URL (required)                                                   |
| `--key <KEY>` / `VEDA_KEY`                                | Workspace key `wk_…` (required)                                         |
| `--foreground`                                            | Block in terminal, no daemon log                                        |
| `--cache-size <MB>`                                       | Read cache size (default 128)                                           |
| `--attr-ttl <SEC>`                                        | Attr cache TTL (default 5)                                              |
| `--allow-other`                                           | Share mount with other UIDs (needs root in some envs)                   |
| `--read-only`                                             | RO mount                                                                |
| `--debug`                                                 | Verbose FUSE-layer logging                                              |
| `--write-mode {sync,writeback}` / `VEDA_FUSE_WRITE_MODE`  | `sync` (default) writes through every flush; `writeback` buffers writes |
| `--write-debounce-ms <N>` / `VEDA_FUSE_WRITE_DEBOUNCE_MS` | Writeback debounce window (default 5000ms)                              |

### Writeback mode (`--write-mode=writeback`)

Buffers writes in an in-memory shadow + debounces commits. vim swap
files, git lockfiles, IDE temp files never reach the server — only
the eventually-durable bytes do. Same-path writes inside the debounce
window coalesce into a single PUT.

**Caveats** (single-user alpha trade-offs):
- In-memory only: a crash within the debounce window loses pending bytes.
- Per-file cap **10 MB**, total cap **50 MB**. Past per-file cap a file
  silently degrades to synchronous writes.
- `unmount` drains pending commits before exiting — a normal shutdown
  loses nothing.

### Summary sidecars (`.abstract` / `.overview`)

Every mounted directory exposes two read-only sidecar files when the
server has summary capability enabled (default):

```sh
cat /mnt/veda/docs/.abstract     # L0 — one-sentence summary
cat /mnt/veda/docs/.overview     # L1 — ~2k-token detailed summary
ls -a /mnt/veda/docs             # includes .abstract / .overview
```

Sidecars are server-generated and read-only (`EROFS` on write). `head`,
`wc`, etc. work normally once `cat` returns. Not-yet-ready summaries
return `summary pending; retry after a few seconds`. Servers built
without summary support return HTTP 501 and the sidecars are hidden
(ENOENT).

`.abstract` / `.overview` directly at `/mnt/veda/` (workspace root)
aren't implemented — use a subdirectory. If the workspace pre-dates
the reserved-name guard and contains real files named `.abstract` or
`.overview`, the sidecar shadows them on read; the real file is still
reachable via `veda cat` / `veda mv` to rename it.

### FUSE error handling

| Symptom                                                              | Meaning + fix                                                                                                       |
|----------------------------------------------------------------------|---------------------------------------------------------------------------------------------------------------------|
| `server health check failed (stat '/' at …)` (exit 1, immediate)     | Preflight failed in daemon child — wrong URL/unreachable server/bad key. Verify `--server` / `--key`                |
| `daemon exited before mount was ready (exit N; check …)`             | Daemon crashed after preflight, before FUSE init. Read `~/.cache/veda-fuse/daemon.log` — full tracing is there      |
| `killed by signal SIGILL` in daemon log                              | Fork/runtime corruption (file a bug)                                                                                |
| Mountpoint hangs / `Transport endpoint is not connected`             | Daemon died after a successful mount. `pgrep -af veda-fuse`; if none, `diskutil unmount force <path>` (Mac) or `fusermount -u <path>` (Linux), re-mount |
| `.abstract` / `.overview` return ENOENT inside a mounted dir         | Server built without summary capability — expected, sidecars are hidden                                             |

### When to mount

| User wants                                            | Mount?                                              |
|-------------------------------------------------------|-----------------------------------------------------|
| Edit a few files with vim / IDE / VS Code             | Yes — writeback mode keeps swap files local         |
| Run `make`, `cargo build`, `rsync -av` over the tree  | Yes                                                 |
| One-off `cp` / `cat` of a single file                 | No — use `veda cp` / `veda cat` directly            |
| Batch upload a local directory                        | No — `veda cp ./dir /remote` is simpler             |
| Browse with `ls -l` and `tree`                        | Yes (read-only mount is enough: `--read-only`)      |
| Search across many files                              | No — use `veda search` / `veda grep` on the server  |

## Decision rules — pick the right command

| User wants                              | Use                                          |
|-----------------------------------------|----------------------------------------------|
| Upload a local file                     | `veda cp local /remote`                      |
| Find old notes by topic                 | `veda search` (default hybrid)               |
| Find exact identifier / string (ranked) | `veda search --mode fulltext`                |
| Find every literal occurrence + line #  | `veda grep` (exhaustive)                     |
| Recall conceptually similar content     | `veda search --mode semantic`                |
| Save tokens on long results             | `veda search --detail-level abstract`        |
| Tabular data with schema                | Collection (not file)                        |
| Filter / aggregate over a collection    | `veda sql` (collection search has no filter) |
| Aggregation across many files           | `veda sql`                                   |
| Edit / grep many files locally          | FUSE mount + native tools                    |
| List / browse files                     | `veda ls`                                    |
| Read specific file                      | `veda cat`                                   |

## Error handling

| Error message                                       | Fix                                                                                                |
|-----------------------------------------------------|----------------------------------------------------------------------------------------------------|
| `invalid path: invalid character in segment: :`     | Drop the `:` — paths are plain `/path`                                                             |
| `no API key configured` / `no workspace selected`   | `veda init` (anon) or `veda init --import-key vk_…\|wk_…`                                          |
| `already onboarded`                                 | `veda init --import-key <key>` (auto-backs up old config), or delete `~/.config/veda/config.toml`  |
| `connection refused` / `Empty reply from server`    | `veda status`; verify `server_url`                                                                 |
| `401 unauthorized`                                  | `veda init --login --email …` or paste a fresh key with `veda init --import-key`                   |
| `Summary not ready yet` (exit 2)                    | Wait ~5s; or `veda cat` for raw content                                                            |
| `Summary unavailable on this server` (exit 3)       | Server has summary disabled; use `veda cat`                                                        |
| 4xx with JSON error body                            | Echo server's `error` field to user                                                                |

FUSE-specific errors are in the "FUSE error handling" sub-section above.

## Don't do

- Upload binaries — PDFs/images aren't supported (no extraction backend).
- Use `veda search` for exact path lookups — use `ls` / `cat`.
- Write to `/` root — pick a semantic prefix (`/notes/`, `/code/`, `/docs/`).
- Repeat the user's password / API key in chat — they live in
  `~/.config/veda/config.toml` (mode 0600) once configured.
- Construct HTTP requests directly — always go through the `veda` CLI.

## Reference

- Repo: https://github.com/jooekong/veda
- Server (alpha): https://veda.dbpaas.dingdongxiaoqu.com
- Changelog: [CHANGELOG.md](./CHANGELOG.md)
- Issues: contact Joe
