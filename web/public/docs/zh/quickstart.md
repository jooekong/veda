# 快速开始

跟着走两分钟，从零到上传第一个文件。

## 1. 拿到账号

在 [首页](/) 点 **Get started anonymously**。页面会展示三样东西：

- `vk_xxx` —— **账号 key**，CLI 用来管理 workspace。
- `wk_xxx` —— **workspace key**，文件 / 搜索操作用它。
- 一个 workspace id。

`wk_` **只显示一次**。离开页面前把两个 key 都复制到安全的地方。

> 后面可以在 **Console** 页点 **Claim account** 加邮箱密码，把匿名账号升级成正式账号。

## 2. 装 CLI

```bash
curl -fsSL https://veda.dbpaas.dingdongxiaoqu.com/install.sh | sh
```

安装器把 `veda` 二进制放到 `/usr/local/bin/`（root）或 `~/.local/bin/`（非 root）。重开终端或 source 你的 rc 文件，让新二进制进 `PATH`。

验证：

```bash
veda --help
```

## 3. 连接到你的账号

把第 1 步拿到的 `vk_` 粘进去：

```bash
veda init --server https://veda.dbpaas.dingdongxiaoqu.com --import-key vk_xxx
```

这一步会写 `~/.config/veda/config.toml`，存好 server URL 和 key。如果文件已存在，会先备份为 `config.toml.bak.<时间戳>`。

## 4. 上传第一个文件

```bash
echo "hello veda" > /tmp/hi.txt
veda cp /tmp/hi.txt /hi.txt
veda ls
veda cat /hi.txt
```

服务器路径都是 workspace 根目录下的绝对路径（`/`）。本地路径用 `./foo` 或 `/abs/foo`。

## 5. 试一下搜索

嵌入是**异步**的，等几秒；没命中过 5 秒再试。

```bash
veda search "greeting"          # 默认 hybrid (向量 + BM25 + RRF)
veda search "hello" --mode fulltext
veda search "concept" --mode semantic
veda grep "hello"               # 字面匹配，同步无延迟，输出 file:line
```

## 6. 试一下 SQL

文件可以当虚拟表查：

```bash
veda sql "SELECT path, size_bytes FROM files ORDER BY created_at DESC LIMIT 5"
```

## 下一步

- [CLI 速查](#/docs/cli) —— 完整命令列表
- [FUSE 挂载](#/docs/fuse) —— 把 Veda workspace 挂成本地目录
- [常见问题](#/docs/troubleshooting)
