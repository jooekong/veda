# FUSE 挂载

把 Veda workspace 挂成本地目录。任何编辑器都能用，改动会自动上传、重新嵌入。

## 安装

`veda-fuse` 跟 `veda` 是两个独立二进制。预编译 release 覆盖 **Linux x86_64** 和 **macOS Intel (x86_64)**；Apple Silicon 暂时需要从源码编译，或者只用 CLI。

```bash
# 一次装好 veda + veda-fuse
curl -fsSL https://veda.dbpaas.dingdongxiaoqu.com/install.sh | sh -s -- --with-fuse
```

宿主机还要装 FUSE：

```bash
# Ubuntu / Debian
sudo apt install fuse3

# RHEL / Fedora / CentOS
sudo dnf install fuse3

# Huawei Cloud EulerOS / openEuler / Kylin / Anolis / TencentOS
sudo yum install fuse3

# macOS（Intel）
brew install --cask macfuse
```

## 挂载

```bash
mkdir -p ~/veda
veda-fuse mount \
  --server https://veda.dbpaas.dingdongxiaoqu.com \
  --key wk_xxx \
  ~/veda
```

请用 **workspace key** (`wk_`)，不是账号 key —— 挂载是针对单个 workspace 的。

挂载后当普通目录用：

```bash
cd ~/veda
ls
echo "今天的笔记" > today.md
cat docs/readme.md
vim today.md
```

## 卸载

跨平台用内置子命令（macOS 走 `umount`，Linux 走 `fusermount3`）：

```bash
veda-fuse umount ~/veda
```

也可以直接用系统命令：`fusermount -u ~/veda`（Linux）/ `umount ~/veda`（macOS）。

## 进阶选项

`veda-fuse mount` 还有一组可选 flag（`veda-fuse mount --help` 看全）：

```bash
veda-fuse mount --server … --key wk_… \
  --write-mode writeback \      # 默认 sync（每次写阻塞上传）；writeback 走本地缓冲 + 防抖批量提交
  --write-debounce-ms 5000 \    # writeback 静默期，默认 5s
  --cache-size 128 \            # 读缓存 MB，默认 128
  --attr-ttl 30 --dir-ttl 60 \  # 属性 / 目录列表缓存 TTL（秒）
  --read-only \                 # 只读挂载
  --allow-other \               # 允许其他用户访问挂载点
  ~/veda
```

`--server` / `--key` 也可由 `$VEDA_SERVER` / `$VEDA_KEY` 或配置文件的活跃 workspace 提供。

## 需要知道的几个行为

- **mtime** 反映真实的上传时间，不是某个常量
- 别的客户端**删除文件**时，本 mount 在 ~120s 内通过 SSE 失效本地 inode
- **大文件**（>256KB）走流式上传；超过 workspace 限额的文件被服务端拒绝，不会静默截断
- **SSE 重连**自动；网络抖动时短暂可能看到旧数据，重连完成即恢复
