# FUSE mount

Mount your Veda workspace as a local directory. Edit files with any editor; changes are synced and re-embedded automatically.

## Install

`veda-fuse` ships as a separate binary from `veda`. Prebuilt releases cover **Linux x86_64**, **macOS Intel (x86_64)**, and **macOS Apple Silicon (aarch64)** (since 0.1.11); the install script picks the right package for your platform.

```bash
# Install veda + veda-fuse together
curl -fsSL https://veda.dbpaas.dingdongxiaoqu.com/install.sh | sh -s -- --with-fuse
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
  --server https://veda.dbpaas.dingdongxiaoqu.com \
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
  --write-mode writeback \      # default sync (each write blocks on upload); writeback buffers locally + debounced batch commits
  --write-debounce-ms 5000 \    # writeback quiet period, default 5s
  --cache-size 128 \            # read cache in MB, default 128
  --attr-ttl 30 --dir-ttl 60 \  # attribute / directory listing cache TTL (seconds)
  --read-only \                 # read-only mount
  --allow-other \               # allow other users to access the mount point
  ~/veda
```

`--server` / `--key` can also come from `$VEDA_SERVER` / `$VEDA_KEY` or the active workspace in the config file.

## Behavior to know

- **mtime** reflects real wall-clock time of the upload (not a synthetic constant)
- Deleting a file from another client invalidates this mount's inode within ~120s via SSE
- **Large files** (> 256KB) are streamed; files over the per-workspace limit are rejected by the server, not silently truncated
- **SSE reconnect** is automatic; if the network drops you may briefly see stale data until reconnection completes
