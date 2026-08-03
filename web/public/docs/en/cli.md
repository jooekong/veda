# CLI reference

Authoritative reference is `veda --help` and `veda <subcommand> --help`. This page lists the commands you'll use most.

> ⚠️ The `veda` CLI only serves **file workspaces** (`kind=fs`). A vector-workspace (`kind=db`) `wk_` returns `400 WORKSPACE_KIND_MISMATCH` on every data command. Bare `veda status` is the one exception — it only pings `/healthz`, so it looks like it works — but `veda status --index` still fails with the same 400. Vector workspaces go through the [vector API](#/docs/vectors).

## Setup

```bash
# Connect with an account key (vk_…)
veda init --server https://veda.ddmc-inc.com --import-key vk_xxx

# Or with a workspace key (wk_…)
veda init --server https://veda.ddmc-inc.com --import-key wk_xxx

# Register a named account from the CLI directly
veda init --email you@example.com --password 'strong-pw'

# Log in to an existing account (keeps its api_key, mints a fresh wk_ for the workspace)
veda init --login --email you@example.com

# Attach an email/password to an anonymous account (its api_key keeps working)
veda init --upgrade --email you@example.com
```

For CI / agents, use non-interactive mode and pass the password by env (`--password` shows up in `ps`):

```bash
VEDA_PASSWORD='strong-pw' veda init --email you@example.com --non-interactive
```

Without `--non-interactive`, the named and login modes still prompt to confirm the server URL, and a pipeline with no tty fails outright. A bare `veda init` on an already-onboarded machine is refused by design — use `veda workspace add` or `--import-key` instead.

### Workspaces (local profiles)

```bash
veda workspace add my-project                 # create a server workspace and save it as a local alias
veda workspace add shared --workspace-id <id> # mint a key against an existing workspace (share across machines)
veda workspace list                           # list local profiles; ★ marks the active one
veda workspace switch my-project              # change the active profile
veda workspace rm my-project                  # removes the local alias only — does not revoke the server-side wk_
veda ws list                                  # ws is shorthand for workspace
```

One-off override: `veda --workspace archive ls /docs` applies to that command only and never touches the config; the alias must already exist.

Config lives at `~/.config/veda/config.toml`. `veda status` shows the current state (server / credential source / active workspace); `veda config show` is a hidden troubleshooting entry point that dumps the full profile list.

Config-free direct connection (CI / scripts / agents — nothing written to disk):

```bash
export VEDA_SERVER=https://veda.ddmc-inc.com
export VEDA_KEY=wk_xxx
veda search "..."               # every data-plane command works as-is
```

Precedence: the `--server` flag > env (`VEDA_SERVER` / `VEDA_KEY`) > config.toml. **There is no `--key` flag on `veda`** — supply the key via `$VEDA_KEY` or the config (`--key` belongs to `veda-fuse mount`). `veda status` labels where credentials came from (env / config).

## Filesystem

```bash
veda cp ./README.md /docs/readme.md          # upload: local → remote
veda cp ./src /code                          # directory upload (recursion auto when src is a dir)
veda cp ./repo /code --no-ignore             # include files your .gitignore excludes
veda cp - /notes/scratch < input.txt         # upload from stdin (use "-" as src)
veda cat /docs/readme.md > ./readme.md       # download: remote → local (redirect cat — cp is upload-only)
veda mv /old.md /archive/old.md
veda rm /tmp                                 # delete (directories recurse by default — no -r; TTY prompts y/N)
veda rm /tmp /scratch/a.md                   # multiple paths; a failure doesn't stop the rest, exit is non-zero
veda mkdir /new-dir                          # create directory
veda append /notes/log "entry"               # append (also supports "-" for stdin)

veda ls
veda ls /docs
veda ls /docs --json                         # one JSON object per line (jq-friendly)
veda cat /docs/readme.md
veda cat /docs/readme.md --range 10:20       # 1-indexed inclusive line range
veda cat /docs/readme.md --head 10           # first 10 lines
veda cat /docs/readme.md --tail 5            # last 5 lines
veda cat /docs/design.pdf --raw > design.pdf # --raw = original bytes (without it, PDF/Word print extracted text)
```

Text and binary both `cp` / `cat` fine (server ≥0.1.15). PDF / Word files get their text extracted and indexed — `cat` prints the extracted text by default, `--raw` fetches the original bytes; other binaries (images / jars / …) are stored but not indexed, and `cat` emits raw bytes (redirect to a file).

### What a directory upload skips

`veda cp <dir>` honours `.gitignore` and `.vedaignore` (same syntax) found **inside the source tree**, plus a built-in list: `.git`, `__pycache__`, `.idea`, `node_modules`, `.DS_Store`.

This filtering is not cosmetic — **every uploaded file costs one embedding call and two LLM summary calls**. Upload a Rust repository without skipping `target/` and hundreds of thousands of build artifacts burn straight through your quota.

Deliberate choices worth knowing:

- **Dotfiles are uploaded.** `.github/`, `.env.example` and `.cursor/rules` are real content; a leading `.` is not a reason to drop them.
- **Ignore files work outside git repositories.** Drop a `.vedaignore` in a plain documentation directory and it applies.
- **Only the source tree is consulted.** Ignore files *above* the source directory, your global gitignore, `.git/info/exclude`, and `.ignore` files (a ripgrep convention) are **not** read — otherwise the same directory would upload different content on different machines.
- `--no-ignore` disables `.gitignore` / `.vedaignore` but keeps the built-in list (`.git/` is never uploaded).

## Workspace layout

```bash
veda layout          # top-level areas, each with a one-line summary and file count
veda layout --json   # structured output for scripts and agents
```

The first command to run against a workspace you don't know: one call for the
whole shape, instead of `veda ls` followed by a `veda abstract` per directory.
Top level only — to go deeper use `veda overview <path>`.

## Search

```bash
veda search "how does auth work"                  # hybrid (vector + BM25 + RRF), the default
veda search "exact term" --mode fulltext
veda search "concept" --mode semantic
veda search "auth" --path /docs                   # scope to a subtree
veda search "auth" --limit 20
veda search "auth" --detail-level abstract        # hits return L0 summary only
veda grep "TODO(joe)" --limit 200                 # literal match (sync, no embedding lag); returns file:line
veda grep "todo" /docs -i                         # restrict to a subtree (positional) + ignore case
```

## Ask (RAG)

```bash
veda ask "how is this system deployed"   # one-shot answer with inline [n] citations + source list
veda ask "…" --path /docs                # restrict retrieval to a subtree
veda ask "…" --json                      # raw JSON (this is the bare data object — parse with jq .citations)
```

The server retrieves and synthesizes the answer itself; may take 10-90s. A 501 (no LLM configured) and a 429 (workspace answer concurrency full) each print their own message, but **both exit 1** — match on the stderr message, not the exit code. Questions are capped at 1024 characters.

## Layered summaries

```bash
veda abstract /docs/readme.md   # L0 one sentence
veda overview /docs/readme.md   # L1 ~2k-token structured prose
```

Generated asynchronously; not-yet-ready returns `Summary not ready yet` (exit 2) — wait a few seconds and retry, or `veda cat` for raw content. When the server has no `[llm]` the whole summary feature is off (exit 3). These two are the **only** custom exit codes in the CLI.

## Structured collections

The schema is one JSON array. `--embed-source` picks the field that gets auto-embedded:

```bash
veda collection create articles \
  --schema '[{"name":"title","type":"string","index":true},
             {"name":"content","type":"string"},
             {"name":"category","type":"string","index":true}]' \
  --embed-source content

# Insert is a JSON ARRAY of rows (not a single object)
veda collection insert articles '[
  {"title":"Intro to Rust","content":"...","category":"tech"},
  {"title":"Pasta","content":"...","category":"food"}
]'

veda collection list
veda collection desc articles
veda collection delete articles
veda collection search articles "systems programming" --limit 5

# For filters / aggregates use SQL (collection search has no --filter)
veda sql "SELECT title FROM articles WHERE category = 'tech' LIMIT 5"
veda sql "SELECT category, COUNT(*) FROM articles GROUP BY category"
```

## Misc

```bash
veda status                     # current config + server reachability (labels env/config credential source)
veda status --index             # indexing progress {pending, processing, dead}
veda status --index --wait      # poll until everything is searchable; non-zero exit on permanent failures (CI gate)
veda config show                # config details
veda --version                  # client version
```
