# FUSE mount

Mount your Veda workspace as a local directory. Edit files with any editor; changes are synced and re-embedded automatically.

## Install

`veda-fuse` ships as a separate binary from `veda`. Prebuilt releases cover **Linux x86_64**, **macOS Intel (x86_64)**, and **macOS Apple Silicon (aarch64)** (since 0.1.11); the install script picks the right package for your platform.

```bash
# Install veda + veda-fuse together
curl -fsSL https://veda.ddmc-inc.com/install.sh | sh -s -- --with-fuse
```

You'll also need FUSE on the host:

```bash
# Ubuntu / Debian
sudo apt install fuse3

# RHEL / Fedora / CentOS
sudo dnf install fuse3

# Huawei Cloud EulerOS / openEuler / Kylin / Anolis / TencentOS
sudo yum install fuse3

# macOS (Intel / Apple Silicon)
brew install --cask macfuse
```

## Mount

```bash
mkdir -p ~/veda
veda-fuse mount \
  --server https://veda.ddmc-inc.com \
  --key wk_xxx \
  ~/veda
```

Use a **workspace key** (`wk_`), not an account key — the mount operates on one workspace.

Use it like a regular directory:

```bash
cd ~/veda
ls
echo "notes from today" > today.md
cat docs/readme.md
vim today.md
```

## Unmount

Use the built-in subcommand on any platform (macOS goes through `umount`, Linux through `fusermount3`):

```bash
veda-fuse umount ~/veda
```

The system commands also work directly: `fusermount -u ~/veda` (Linux) / `umount ~/veda` (macOS).

## Advanced options

`veda-fuse mount` takes a set of optional flags (see `veda-fuse mount --help` for the full list):

```bash
veda-fuse mount --server … --key wk_… \
  --workspace myws \            # use this config-file alias instead of active_workspace; only a fallback source for --key
  --foreground \                # run in the foreground; the default is a background daemon
  --write-mode writeback \      # default sync (writes buffer in memory; close/fsync blocks on a whole-file upload); writeback buffers locally + debounced batch commits
  --write-debounce-ms 5000 \    # writeback quiet period, default 5s
  --cache-size 128 \            # read cache in MB, default 128
  --attr-ttl 30 --dir-ttl 60 \  # attribute / directory listing cache TTL (seconds)
  --read-only \                 # read-only mount
  --allow-other \               # allow other users to access the mount point; on Linux this also needs user_allow_other in /etc/fuse.conf
  --debug \                     # raises the log level to debug, nothing else
  ~/veda
```

`--server` / `--key` can also come from `$VEDA_SERVER` / `$VEDA_KEY` or the active workspace in the config file; `--write-mode` / `--write-debounce-ms` likewise accept `$VEDA_FUSE_WRITE_MODE` / `$VEDA_FUSE_WRITE_DEBOUNCE_MS`.

The default is a background daemon: `mount` prints a daemon log path first, and a failed mount explains itself in that file (otherwise the command looks like it did nothing). Also, AutoUnmount is only passed in `--foreground` mode — but the daemon still installs a SIGINT/SIGTERM handler that unmounts on the way out. Only SIGKILL or a hard crash leaves the mount behind; then run `veda-fuse umount` yourself.

## Summary sidecars (`.abstract` / `.overview`)

Every directory **except the workspace root** shows two extra synthetic entries in `ls -a`: `.abstract` (the L0 summary of that directory) and `.overview` (the L1 overview) — and none at all if the deployment has no LLM configured, since the mount probes for that at startup. They are not real files and consume no storage:

```bash
cat docs/.abstract
cat docs/.overview
```

- **Read-only**: any write or truncate returns `EROFS`
- While a summary is still being generated, reading one returns the placeholder line `summary pending; retry after a few seconds` — retry a few seconds later
- `rm` on a sidecar is **deliberately a silent no-op success** so `rm -rf` completes cleanly; the name "reappears" on the next `ls`, because the summary lives as long as its directory does. `rmdir` on a sidecar returns `ENOTDIR` (it's a file, not a directory)

## Behavior to know

- **mtime** reflects real wall-clock time of the upload (not a synthetic constant)
- Changes from other clients (create / update / delete / rename) arrive over SSE and invalidate this mount's read / attr / dir caches **on receipt**, normally within a second. The 120s figure is a hard cap on each SSE connection's total lifetime, not an invalidation latency — healthy connections are recycled at 120s too, resuming from `since_id`, and a silently dead connection is bounded by the same cap
- **No streaming for large files**: files over 1MB bypass the read cache, so every `read()` issues its own HTTP Range request; in sync mode the whole file stays in memory and is re-uploaded in full on every close. The HTTP client has a hard 30s timeout, so a large upload that runs long fails with EIO. Files over the per-workspace limit are rejected by the server, not silently truncated
- **SSE reconnect** is automatic; if the network drops you may briefly see stale data until reconnection completes
- **The cost of writeback**: the buffer is memory-only. A crash or hard kill inside the debounce window loses uncommitted writes, and a failed commit PUT is only logged, never surfaced (your `close()` already returned success). The entry stays Dirty, so it is retried on the next write to that file and again at unmount — but there is no retry scheduler, so nothing happens in between. Use the default `sync` if you need write-time confirmation
- **Writeback has size caps**: 10MB per file, 50MB across all buffers. A file that grows past 10MB silently degrades to sync writes (no error — it just loses the debounce); a write that would breach the 50MB total returns `ENOSPC`
- **Incomplete POSIX coverage**: extended attributes are not supported at all (`getxattr` / `setxattr` / `listxattr` / `removexattr` all return `ENOSYS`); `chmod` / `chown` / `utimes` succeed but do nothing — only `size` (truncate) is honored in setattr; permissions are synthesized constants, 0755 for directories and 0644 for files; hard links and symlinks are unsupported (`link` / `symlink` return `EPERM`); `access()` always says yes; and `statfs` reports a fake ~512 GiB free, so `df` and friends never see your real quota
