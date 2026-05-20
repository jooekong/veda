# FUSE mount

Mount your Veda workspace as a local directory. Edit files with any editor; changes are synced and re-embedded automatically.

## Install

`veda-fuse` ships as a separate binary from `veda`. Prebuilt releases cover **Linux x86_64** and **macOS Intel (x86_64)**. Apple Silicon needs a source build, or use the CLI alone for now.

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

# macOS (Intel)
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

```bash
fusermount -u ~/veda    # Linux
```

## Behavior to know

- **mtime** reflects real wall-clock time of the upload (not a synthetic constant)
- Deleting a file from another client invalidates this mount's inode within ~120s via SSE
- **Large files** (> 256KB) are streamed; files over the per-workspace limit are rejected by the server, not silently truncated
- **SSE reconnect** is automatic; if the network drops you may briefly see stale data until reconnection completes
