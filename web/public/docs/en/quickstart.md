# Quickstart

Veda has two kinds of workspaces. Decide which one you need first, then follow the matching section:

- **File Workspace** (`kind=fs`) — store files, automatic embedding, semantic search + SQL, mountable as a local directory. Personal knowledge bases, agent memory, and code search go this route; the CLI is the smoothest way in.
- **Vector Workspace** (`kind=db`) — Pinecone-style managed vector retrieval: write text, server-side embedding, retrieve by meaning. Business apps go this route via REST / SDK.

---

## A. File Workspace: CLI in 2 minutes

### 1. Get an account

On the [home page](/), click **Get started anonymously**. The page shows three things:

- `vk_xxx` — your **account key**, used to manage workspaces.
- `wk_xxx` — your **workspace key**, used for file / search operations.
- A workspace id.

The `wk_` is shown **only once** — copy both keys somewhere safe before leaving the page. Later you can add an email + password from the **Console** by clicking **Claim account**, upgrading the anonymous account into a full one.

### 2. Install the CLI

```bash
curl -fsSL https://veda.dbpaas.dingdongxiaoqu.com/install.sh | sh
```

The binary goes into `/usr/local/bin/` (root) or `~/.local/bin/` (non-root). Reopen your terminal so it's on `PATH`, then verify with `veda --help`.

### 3. Connect the CLI to your account

```bash
veda init --server https://veda.dbpaas.dingdongxiaoqu.com --import-key vk_xxx
```

Config is written to `~/.config/veda/config.toml` (an existing file is backed up to `config.toml.bak.<timestamp>` first).

### 4. Upload, read, list

```bash
echo "hello veda" > /tmp/hi.txt
veda cp /tmp/hi.txt /hi.txt        # upload: local → remote
veda ls
veda cat /hi.txt                   # read a remote file (download = veda cat /remote > local)
```

Server-side paths are absolute under your workspace root (`/`).

### 5. Search

Embedding is **asynchronous** — give a fresh upload a few seconds, and retry after ~5s if it doesn't hit.

```bash
veda search "greeting"             # hybrid (vector + BM25 + RRF), default
veda search "hello" --mode fulltext
veda search "concept" --mode semantic
veda grep "hello"                  # literal match, sync (no delay), returns file:line
```

### 6. Query it like a database

Files are queryable as a virtual table:

```bash
veda sql "SELECT path, size_bytes FROM files ORDER BY created_at DESC LIMIT 5"
```

**Next**: [CLI reference](#/docs/cli) (full command list) · [FUSE mount](#/docs/fuse) (mount as a local directory) · [Full reference](#/docs/reference)

---

## B. Vector Workspace: app integration

The Vector Workspace is for business apps, over HTTP / SDK. `<BASE>` is your deployment address, e.g. `https://veda.dbpaas.dingdongxiaoqu.com`.

A business app usually holds **just one `wk_`** (issued by the platform / console for a given db workspace). With it, hit the data plane directly:

```bash
BASE=https://veda.dbpaas.dingdongxiaoqu.com
WK=wk_...        # workspace key issued to you by the platform (no workspace_id in the request body)

# upsert: text is embedded server-side automatically
curl -sX POST $BASE/v1/vectors/upsert \
  -H "authorization: Bearer $WK" -H 'content-type: application/json' \
  -d '{"records":[
        {"id":"sku-1","text":"Air Jordan 1","meta":{"price":1299}},
        {"id":"sku-2","text":"running shoes","meta":{"price":499}}
      ]}'

# search: hybrid (vector + BM25) by default, optional meta filter
curl -sX POST $BASE/v1/vectors/search \
  -H "authorization: Bearer $WK" -H 'content-type: application/json' \
  -d '{"query":"sneakers under 1500","top_k":5,
       "filter":{"must":[{"field":"meta.price","op":"lt","value":1500}]}}'
```

Check `score_type` before reading scores (`rrf` / `cosine` / `bm25` are not comparable across types).

> ⚠️ Don't use anonymous onboarding (`POST /v1/accounts/anonymous`) for the Vector Workspace — it creates `kind=fs`, which the vector endpoints can't use.

<details>
<summary>Provision a Vector Workspace yourself (with an account vk_)</summary>

If you hold an account `vk_` and want to create the workspace and issue keys yourself:

```bash
ACCOUNT_KEY=vk_...

# create a db workspace (auto-bootstraps the default dataset + Milvus collection)
curl -sX POST $BASE/v1/workspaces \
  -H "authorization: Bearer $ACCOUNT_KEY" -H 'content-type: application/json' \
  -d '{"name":"prod-index","kind":"db"}'        # → data.id is the <ws_id>

# issue a data-plane wk_ for it (plaintext shown only once)
curl -sX POST $BASE/v1/workspaces/<ws_id>/keys \
  -H "authorization: Bearer $ACCOUNT_KEY" -H 'content-type: application/json' \
  -d '{"name":"search-svc","permission":"readwrite"}'
```

</details>

**Next**: [Vector Workspace API](#/docs/vectors) (full API / Filter DSL / limits) · [Full reference](#/docs/reference)

---

## Stuck?

See [Troubleshooting](#/docs/troubleshooting), or open an issue at [git.ddxq.mobi/middleware/dbpaas/veda](http://git.ddxq.mobi/middleware/dbpaas/veda).
