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

```bash
fusermount -u ~/veda    # Linux
```

## 需要知道的几个行为

- **mtime** 反映真实的上传时间，不是某个常量
- 别的客户端**删除文件**时，本 mount 在 ~120s 内通过 SSE 失效本地 inode
- **大文件**（>256KB）走流式上传；超过 workspace 限额的文件被服务端拒绝，不会静默截断
- **SSE 重连**自动；网络抖动时短暂可能看到旧数据，重连完成即恢复
