# CLI reference

Authoritative reference is `veda --help` and `veda <subcommand> --help`. This page lists the commands you'll use most.

## Setup

```bash
# Connect with an account key (vk_…)
veda init --server https://veda.dbpaas.dingdongxiaoqu.com --import-key vk_xxx

# Or with a workspace key (wk_…)
veda init --server https://veda.dbpaas.dingdongxiaoqu.com --import-key wk_xxx

# Register a named account from the CLI directly
veda init --email you@example.com --password 'strong-pw'

# Log in to an existing account (gets a fresh login key)
veda init --login --email you@example.com

# Add another workspace under the current account
veda workspace add my-project
```

Config lives at `~/.config/veda/config.toml`. Inspect with `veda config show`.

## Filesystem

```bash
veda cp ./README.md /docs/readme.md          # upload
veda cp /docs/readme.md ./readme.md          # download (first arg starts with / → remote)
veda cp /docs/readme.md /backup/readme       # server-side copy
veda cp ./src /code                          # directory upload (recursion auto when src is a dir)
veda cp - /notes/scratch < input.txt         # from stdin (use "-" as src)
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

Server paths are absolute under your workspace root (`/`). Local paths use `./foo` or `/abs/foo`. UTF-8 text only — binaries (PDF / images) are rejected client-side.

## Search

```bash
veda search "how does auth work"                  # hybrid (vector + BM25 + RRF)
veda search "exact term" --mode fulltext
veda search "concept" --mode semantic
veda search "auth" --path /docs                   # scope to a subtree
veda search "auth" --limit 20
veda search "auth" --detail-level abstract        # hits return L0 summary only
veda grep "TODO(joe)" --limit 200                 # literal match (sync, no embedding lag); returns file:line
```

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
veda status                     # current config + server reachability
veda config show                # config details
veda --version                  # client version
```
