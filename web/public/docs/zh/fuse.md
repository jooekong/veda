# FUSE 挂载

把 Veda workspace 挂成本地目录。任何编辑器都能用，改动会自动上传、重新嵌入。

## 安装

`veda-fuse` 跟 `veda` 是两个独立二进制。预编译 release 覆盖 **Linux x86_64**、**macOS Intel (x86_64)** 和 **macOS Apple Silicon (aarch64)**（0.1.11 起），安装脚本自动按平台选包。

```bash
# 一次装好 veda + veda-fuse
curl -fsSL https://veda.ddmc-inc.com/install.sh | sh -s -- --with-fuse
```

宿主机还要装 FUSE：

```bash
# Ubuntu / Debian
sudo apt install fuse3

# RHEL / Fedora / CentOS
sudo dnf install fuse3

# Huawei Cloud EulerOS / openEuler / Kylin / Anolis / TencentOS
sudo yum install fuse3

# macOS（Intel / Apple Silicon）
brew install --cask macfuse
```

## 挂载

```bash
mkdir -p ~/veda
veda-fuse mount \
  --server https://veda.ddmc-inc.com \
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
  --workspace myws \            # 用配置文件里该别名的 workspace 顶替 active_workspace，仅作为 --key 的兜底
  --foreground \                # 前台运行，默认后台 daemon
  --write-mode writeback \      # 默认 sync（写入内存缓冲，close/fsync 时阻塞整文件上传）；writeback 走本地缓冲 + 防抖批量提交
  --write-debounce-ms 5000 \    # writeback 静默期，默认 5s
  --cache-size 128 \            # 读缓存 MB，默认 128
  --attr-ttl 30 --dir-ttl 60 \  # 属性 / 目录列表缓存 TTL（秒）
  --read-only \                 # 只读挂载
  --allow-other \               # 允许其他用户访问挂载点；Linux 上还需在 /etc/fuse.conf 里打开 user_allow_other
  --debug \                     # 只把日志级别调到 debug，不改变任何行为
  ~/veda
```

`--server` / `--key` 也可由 `$VEDA_SERVER` / `$VEDA_KEY` 或配置文件的活跃 workspace 提供；`--write-mode` / `--write-debounce-ms` 同样认 `$VEDA_FUSE_WRITE_MODE` / `$VEDA_FUSE_WRITE_DEBOUNCE_MS`。

默认是后台 daemon 模式：`mount` 会先打印一行 daemon 日志路径，挂载失败的原因写在那个文件里（不然命令行上看着像什么都没发生）。另外只有 `--foreground` 才会带上 AutoUnmount —— 但 daemon 模式同样装了 SIGINT/SIGTERM 处理器，收到信号时会卸载。只有 SIGKILL 或硬崩溃才会留下残挂载，那时手动 `veda-fuse umount`。

## 摘要 sidecar（`.abstract` / `.overview`）

**除 workspace 根目录外**，每个目录的 `ls -a` 里都会多出两个合成条目：`.abstract`（该目录的 L0 摘要）和 `.overview`（L1 概览）；如果部署没配 LLM，挂载时的能力探测会发现，这两个条目根本不出现。它们不是真实文件，也不占任何存储：

```bash
cat docs/.abstract
cat docs/.overview
```

- **只读**：写入 / 截断一律 `EROFS`
- 摘要还没生成好时，读回来是一行占位文本 `summary pending; retry after a few seconds`，过几秒重试即可
- `rm` 掉 sidecar 是**故意做成静默成功但什么都不干**的，好让 `rm -rf` 能跑完；名字会在下次 `ls` 时"重新出现"，因为摘要跟着目录走。对 sidecar 用 `rmdir` 则返回 `ENOTDIR`（它是文件不是目录）

## 需要知道的几个行为

- **mtime** 反映真实的上传时间，不是某个常量
- 别的客户端的改动（新增 / 更新 / 删除 / 重命名）通过 SSE 推过来，**收到即失效**本地的读 / 属性 / 目录缓存，通常在一秒内。120s 是单条 SSE 连接**整体寿命**的硬上限，不是失效延迟 —— 健康连接到 120s 也照样被回收重连（按 `since_id` 续上），静默断掉的连接同样由这个上限兜底
- **大文件没有流式上传**：超过 1MB 的文件读取时绕过读缓存，每次 `read()` 直接发一个 HTTP Range 请求；sync 模式下整个文件常驻内存，每次 close 都整份重传一遍。HTTP 客户端有 30s 硬超时，大文件传太慢会以 EIO 失败。超过 workspace 限额的文件被服务端拒绝，不会静默截断
- **SSE 重连**自动；网络抖动时短暂可能看到旧数据，重连完成即恢复
- **writeback 的代价**：缓冲只在内存里。防抖窗口内进程崩溃 / 被强杀，未提交的写会丢；提交 PUT 失败只写日志、不会报错给你（`close()` 早已返回成功）；该条目保持 Dirty，下次写这个文件时会重试，unmount 时也会再试一次——但没有重试调度器，中间这段时间不会自己重来。要写入即确认，用默认 `sync`
- **writeback 有容量上限**：单文件 10MB、所有缓冲合计 50MB。单个文件写超 10MB 会静默退回 sync 写（不报错，只是没了防抖）；总量超 50MB 的写直接返回 `ENOSPC`
- **POSIX 覆盖不全**：完全不支持扩展属性（`getxattr` / `setxattr` / `listxattr` / `removexattr` 一律 `ENOSYS`）；`chmod` / `chown` / `utimes` 会"成功"但什么都不做，setattr 里只有 `size`（截断）真正生效；权限是合成的固定值，目录 0755、文件 0644；硬链接和符号链接不支持（`link` / `symlink` 返回 `EPERM`）；`access()` 恒返回通过；`statfs` 报的是约 512 GiB 空闲的假数据，所以 `df` 之类的工具看不到真实配额
