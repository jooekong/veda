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
   curl -fL https://git.ddxq.mobi/middleware/dbpaas/veda/-/raw/main/install.sh | sh
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
   veda use my-project
   ```

5. **Verify**:
   ```sh
   veda ls /
   ```
   Empty listing or existing files is fine. Errors mean a step above failed.

## Core operations

### File operations

Remote paths use `:/path` syntax (the `:` prefix means "in current workspace").

```sh
veda cp ./README.md :/docs/readme.md            # upload
veda cp ./report.pdf :/docs/report.pdf          # PDF — text auto-extracted
veda cp - :/notes/scratch < some-input          # from stdin
veda ls :/docs                                  # list dir
veda cat :/docs/readme.md                       # full content
veda cat :/docs/readme.md --lines 10:20         # line range
veda mv :/old-name :/new-name                   # rename
veda rm :/path -r                               # delete
veda mkdir :/new-dir                            # create directory
veda append :/notes/log "new entry"             # append to file
```

### Search

Three modes; default `hybrid` fuses semantic + BM25 with RRF. Pick the right mode:

```sh
veda search "how does authentication work"                    # hybrid (best general)
veda search "fn parse_token" --mode fulltext                  # exact strings, identifiers
veda search "code about retry semantics" --mode semantic      # conceptual only
veda search "summary" --detail-level abstract                 # save tokens — return summaries
```

`--detail-level`: `abstract` (cheapest), `overview`, `full` (default). `--limit N` sets result count.

### Summary

```sh
veda summary :/path/to/file.md       # L0/L1 auto-generated summary for a file
veda summary :/                      # workspace-level summary
```

### Structured collections

Schema-first, vector-native. Field flags: `index` = filterable, `embed` = auto-embed this string field.

```sh
veda collection create articles \
  --field "title:string:index" \
  --field "content:string:embed" \
  --field "category:string:index"

veda collection insert articles \
  --data '{"title":"Intro to Rust","content":"Rust is a systems language...","category":"tech"}'

veda collection search articles "systems programming" \
  --filter "category == 'tech'" --limit 10

# Raw vector collection (you provide the vectors)
veda collection create my-vecs --dim 768 --type raw
veda insert --vector "[0.1,0.2,0.3]" --payload '{"label":"x"}'
```

### SQL queries

DataFusion engine, standard SQL across collections.

```sh
veda sql "SELECT title, category FROM articles WHERE category = 'tech' LIMIT 5"
veda sql "SELECT category, COUNT(*) FROM articles GROUP BY category"
```

### FUSE mount (only if installed with --with-fuse)

```sh
veda-fuse mount /mnt/veda --server $SERVER --key $WORKSPACE_KEY
# now use ripgrep, ls, $EDITOR, etc. on /mnt/veda
veda-fuse umount /mnt/veda
```

## Decision rules — pick the right command

| User wants                             | Use                                      |
|----------------------------------------|------------------------------------------|
| Upload a local file                    | `veda cp local :/remote`                 |
| Find old notes by topic                | `veda search` (default hybrid)           |
| Find exact identifier / string         | `veda search --mode fulltext`            |
| Recall conceptually similar content    | `veda search --mode semantic`            |
| Save tokens on long results            | `veda search --detail-level abstract`    |
| Tabular data with schema, filterable   | Collection (not file)                    |
| Aggregation across many files          | `veda sql`                               |
| Edit / grep many files locally         | FUSE mount + native tools                |
| List / browse files                    | `veda ls`                                |
| Read specific file                     | `veda cat`                               |

## Error handling

| Error message                          | Meaning              | Fix                                              |
|----------------------------------------|---------------------|--------------------------------------------------|
| `no API key configured`                | Not authenticated    | Ask user to run `veda account create` or `login` |
| `no workspace selected`                | No active workspace  | `veda workspace list` → `veda use <name>`        |
| `connection refused`                   | Server unreachable   | `veda config show`; verify server URL            |
| `401 unauthorized`                     | API key invalid      | `veda account login` to refresh                  |
| 4xx with JSON error body               | API error            | Echo server's `message` field to user            |

## Don't do

- Don't store large binaries — PDFs/images keep extracted text only, the original is dropped.
- Don't use `veda search` for exact path lookups — use `ls` / `cat` instead.
- Don't write to `:/` root — pick a semantic prefix like `:/notes/`, `:/code/`, `:/docs/`.
- Don't repeat the user's password / API key in chat — they live in `~/.config/veda/config.toml` (mode 0600) once configured.
- Don't construct HTTP requests directly — always go through the `veda` CLI.

## Reference

- Repo: https://git.ddxq.mobi/middleware/dbpaas/veda
- Server (alpha): http://10.79.51.161:3000
- Issues: contact Joe
