# CLI reference

Authoritative reference is `veda --help` and `veda <subcommand> --help`. This page lists the commands you'll use most.

## Setup

```bash
# Connect with an account key (vk_…)
veda init --server https://veda.ddmc-inc.com --import-key vk_xxx

# Or with a workspace key (wk_…)
veda init --server https://veda.ddmc-inc.com --import-key wk_xxx

# Register a named account from the CLI directly
veda init --email you@example.com --password 'strong-pw'

# Log in to an existing account (gets a fresh login key)
veda init --login --email you@example.com

# Add another workspace under the current account
veda workspace add my-project
```

Config lives at `~/.config/veda/config.toml`. Inspect with `veda config show`.

Config-free direct connection (CI / scripts / agents — nothing written to disk):

```bash
export VEDA_SERVER=https://veda.ddmc-inc.com
export VEDA_KEY=wk_xxx
veda search "..."               # every data-plane command works as-is
```

Precedence: `--server` / `--key` flags > env > config.toml (same names and order as `veda-fuse`); `veda status` labels where credentials came from (env / config).

## Filesystem

```bash
veda cp ./README.md /docs/readme.md          # upload: local → remote
veda cp ./src /code                          # directory upload (recursion auto when src is a dir)
veda cp - /notes/scratch < input.txt         # upload from stdin (use "-" as src)
veda cat /docs/readme.md > ./readme.md       # download: remote → local (redirect cat — cp is upload-only)
veda mv /old.md /archive/old.md
veda rm /tmp                                 # delete (directories recurse by default — no -r; TTY prompts y/N)
veda mkdir /new-dir                          # create directory
veda append /notes/log "entry"               # append (also supports "-" for stdin)

veda ls
veda ls /docs
veda ls /docs --json                         # one JSON object per line (jq-friendly)
veda cat /docs/readme.md
veda cat /docs/readme.md --range 10:20       # 1-indexed inclusive line range
veda cat /docs/readme.md --head 10           # first 10 lines
veda cat /docs/readme.md --tail 5            # last 5 lines
```

Text and binary both `cp` / `cat` fine (server ≥0.1.15). PDF / Word files get their text extracted and indexed — `cat` prints the extracted text by default, `--raw` fetches the original bytes; other binaries (images / jars / …) are stored but not indexed, and `cat` emits raw bytes (redirect to a file).

## Search

```bash
veda search "how does auth work"                  # hybrid (vector + BM25 + RRF), the default
veda search "exact term" --mode fulltext
veda search "concept" --mode semantic
veda search "auth" --path /docs                   # scope to a subtree
veda search "auth" --limit 20
veda search "auth" --detail-level abstract        # hits return L0 summary only
veda grep "TODO(joe)" --limit 200                 # literal match (sync, no embedding lag); returns file:line
```

## Ask (RAG)

```bash
veda ask "how is this system deployed"   # one-shot answer with inline [n] citations + source list
veda ask "…" --path /docs                # restrict retrieval to a subtree
veda ask "…" --json                      # raw JSON (parse citations with jq)
```

The server retrieves and synthesizes the answer itself; may take 10-90s. Returns 501 when the server has no LLM configured and 429 when the workspace's answer concurrency is full — distinct exit codes for scripts.

## Layered summaries

```bash
veda abstract /docs/readme.md   # L0 one sentence
veda overview /docs/readme.md   # L1 ~2k-token structured prose
```

Generated asynchronously; not-yet-ready returns `Summary not ready yet` (exit 2) — wait a few seconds and retry, or `veda cat` for raw content.

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
