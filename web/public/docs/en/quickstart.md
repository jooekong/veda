# Quickstart

This walks you from zero to your first uploaded file in about 2 minutes.

## 1. Get an account

On the [home page](/), click **Get started anonymously**. The page shows three things:

- `vk_xxx` — your **account key**. Used by the CLI to manage workspaces.
- `wk_xxx` — your **workspace key**. Used for actual file/search operations.
- A workspace id.

The workspace key is shown **only once**. Copy both keys somewhere safe before leaving the page.

> Anonymous accounts are free and fine for trying things. Later you can add an email + password from the **Console** page by clicking **Claim account**.

## 2. Install the CLI

```bash
curl -fsSL https://veda.dbpaas.dingdongxiaoqu.com/install.sh | sh
```

The installer drops a `veda` binary into `/usr/local/bin/` (root) or `~/.local/bin/` (non-root). Reopen your shell or `source` your rc file so the new binary is on `PATH`.

Check it works:

```bash
veda --help
```

## 3. Connect the CLI to your account

Paste the `vk_` from step 1 into:

```bash
veda init --server https://veda.dbpaas.dingdongxiaoqu.com --import-key vk_xxx
```

This writes `~/.config/veda/config.toml` with your server URL and key. If the file already exists it is backed up to `config.toml.bak.<timestamp>` first.

## 4. Upload your first file

```bash
echo "hello veda" > /tmp/hi.txt
veda cp /tmp/hi.txt /hi.txt
veda ls
veda cat /hi.txt
```

Paths on the server are absolute under your workspace root (`/`). Local paths use normal `./foo` or `/abs/foo`.

## 5. Try search

Veda chunks and embeds every text file automatically. This is **asynchronous** — give it a few seconds, then retry after ~5s if it doesn't hit.

```bash
veda search "greeting"          # hybrid (vector + BM25 + RRF), default
veda search "hello" --mode fulltext
veda search "concept" --mode semantic
veda grep "hello"               # literal match, sync, returns file:line
```

## 6. Try SQL

Files are queryable as a virtual table:

```bash
veda sql "SELECT path, size_bytes FROM files ORDER BY created_at DESC LIMIT 5"
```

## Next

- [CLI reference](#/docs/cli) — full command list
- [FUSE mount](#/docs/fuse) — mount your Veda workspace as a local directory
- [Troubleshooting](#/docs/troubleshooting)
