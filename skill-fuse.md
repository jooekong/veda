---
name: veda-fuse
description: |
  FUSE filesystem client for Veda. Use when the user wants to mount their
  workspace as a local directory, edit files with native tools (vim, IDE,
  rsync, make), or browse via `ls -l`. Companion to skill.md — only relevant
  when `veda-fuse` is installed alongside the `veda` CLI.
---

# Using veda-fuse

`veda-fuse` mounts a Veda workspace as a local FUSE filesystem so native
tools (vim, IDE, `rsync`, `make`, `ls -l`) can read and write workspace
files like a normal directory tree. Companion to [skill.md](./skill.md);
the CLI handles auth + structured ops, FUSE handles file-as-file
interaction.

## Install

macOS needs `brew install --cask macfuse` first. Linux uses libfuse3
(`sudo apt-get install libfuse3-3 fuse3` on Debian/Ubuntu).

```sh
curl -fL https://veda.dbpaas.dingdongxiaoqu.com/install.sh | sh -s -- --with-fuse
```

The GitLab release stream doesn't ship `veda-fuse` for
`aarch64-apple-darwin` (cross-link blocker — `pkg-config` won't resolve
macFUSE across architectures). Apple Silicon users build from source or
stick to the CLI.

## Mount / unmount

```sh
veda-fuse mount --server $SERVER --key $WORKSPACE_KEY /mnt/veda
# → prints "veda: daemon log → ~/.cache/veda-fuse/daemon.log"
# → prints "veda: mounted (pid 12345)" and returns immediately
veda-fuse umount /mnt/veda
```

Default is **daemon mode**: the command forks, detaches stdio, and the
parent returns as soon as the FUSE mount handshake completes. Pass
`--foreground` to keep the process attached (logs go to stdout/stderr
instead of the daemon log file).

## Mount flags / env vars

| Flag / env                                              | Effect                                                                  |
|---------------------------------------------------------|-------------------------------------------------------------------------|
| `--server <URL>` / `VEDA_SERVER`                        | Server URL (required)                                                   |
| `--key <KEY>` / `VEDA_KEY`                              | Workspace key `wk_…` (required)                                         |
| `--foreground`                                          | Block in terminal, no daemon log                                        |
| `--cache-size <MB>`                                     | Read cache size (default 128)                                           |
| `--attr-ttl <SEC>`                                      | Attr cache TTL (default 5)                                              |
| `--allow-other`                                         | Share mount with other UIDs (needs root in some envs)                   |
| `--read-only`                                           | RO mount                                                                |
| `--debug`                                               | Verbose FUSE-layer logging                                              |
| `--write-mode {sync,writeback}` / `VEDA_FUSE_WRITE_MODE`| `sync` (default) writes through every flush; `writeback` buffers writes |
| `--write-debounce-ms <N>` / `VEDA_FUSE_WRITE_DEBOUNCE_MS` | Writeback debounce window (default 5000ms)                            |

## Writeback mode (`--write-mode=writeback`)

Buffers writes in an in-memory shadow + debounces commits. vim swap files,
git lockfiles, IDE temp files never reach the server — only the eventually-
durable bytes do. Same-path writes inside the debounce window coalesce
into a single PUT.

**Caveats** (single-user alpha trade-offs):
- In-memory only: a crash within the debounce window loses pending bytes.
- Per-file cap **10 MB**, total cap **50 MB**. Past per-file cap a file
  silently degrades to synchronous writes.
- `unmount` drains pending commits before exiting — a normal shutdown
  loses nothing.

## Summary sidecars (`.abstract` / `.overview`)

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

`.abstract` / `.overview` directly at `/mnt/veda/` (workspace root) aren't
implemented — use a subdirectory. If the workspace pre-dates the
reserved-name guard and contains real files named `.abstract` or
`.overview`, the sidecar shadows them on read; the real file is still
reachable via `veda cat` / `veda mv` to rename it.

## Error handling

| Symptom                                                                            | Meaning + fix                                                                                                       |
|------------------------------------------------------------------------------------|---------------------------------------------------------------------------------------------------------------------|
| `server health check failed (stat '/' at …)` (exit 1, immediate)                   | Preflight failed in daemon child — wrong URL/unreachable server/bad key. Verify `--server` / `--key`                |
| `daemon exited before mount was ready (exit N; check …)`                           | Daemon crashed after preflight, before FUSE init. Read `~/.cache/veda-fuse/daemon.log` — full tracing is there      |
| `killed by signal SIGILL` in daemon log                                            | Fork/runtime corruption (file a bug)                                                                                |
| Mountpoint hangs / `Transport endpoint is not connected`                           | Daemon process died after a successful mount. `pgrep -af veda-fuse`; if none, `diskutil unmount force <path>` (Mac) or `fusermount -u <path>` (Linux), re-mount |
| `.abstract` / `.overview` return ENOENT inside a mounted dir                       | Server built without summary capability — expected, sidecars are hidden                                             |

The daemon log captures the full startup trace — SSE connection, FUSE
init, any panic — in append mode at `~/.cache/veda-fuse/daemon.log`.

## Decision: when to mount

| User wants                                            | Mount?                                              |
|-------------------------------------------------------|-----------------------------------------------------|
| Edit a few files with vim / IDE / VS Code             | Yes — writeback mode keeps swap files local         |
| Run `make`, `cargo build`, `rsync -av` over the tree  | Yes                                                 |
| One-off `cp` / `cat` of a single file                 | No — use `veda cp` / `veda cat` directly            |
| Batch upload a local directory                        | No — `veda cp ./dir /remote` is simpler             |
| Browse with `ls -l` and `tree`                        | Yes (read-only mount is enough: `--read-only`)      |
| Search across many files                              | No — use `veda search` / `veda grep` on the server  |

## Reference

- Skill index: [skill.md](./skill.md)
- Repo: https://github.com/jooekong/veda
- Issues: contact Joe
