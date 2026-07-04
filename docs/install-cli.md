# Veda CLI 安装 SOP

终端用户在本机安装 `veda` CLI 的标准流程。
适用平台：macOS（Apple Silicon / Intel）、Linux x86_64。
服务端 / VM 部署不在本文范围，见 [deploy.md](deploy.md)。

---

## TL;DR

`install.sh` 从内网 GitLab（`git.ddxq.mobi`）拉对应平台的预编译产物，校验 sha256，装进 PATH，并自动解析最新版本。

**mac 必须先清掉 http 代理**（见[下文](#为什么-mac-要清代理)），否则 TLS 握手直接失败。

```sh
# 入口 A：远程一键（无需源码，最通用）
env -u http_proxy -u https_proxy -u HTTP_PROXY -u HTTPS_PROXY \
  sh -c 'curl -fL https://veda.ddmc-inc.com/install.sh | sh'

# 入口 B：本地仓库（已 clone veda 源码时）
env -u http_proxy -u https_proxy -u HTTP_PROXY -u HTTPS_PROXY \
  sh install.sh
```

验证：

```sh
veda --version          # → veda 0.1.16
```

两个入口等价：都执行同一份 `install.sh`，从 GitLab 拉同一批产物。Linux 无代理环境可去掉 `env -u …` 前缀。

---

## 为什么 mac 要清代理

mac 上 Clash 的系统代理（`http_proxy=127.0.0.1:8082`）会把内网域名 `git.ddxq.mobi` 的流量也劫走，导致：

```
curl: (35) LibreSSL SSL_connect: SSL_ERROR_SYSCALL in connection to git.ddxq.mobi:443
```

`install.sh` 从内网 GitLab 下载产物，必须**直连**（让流量走 Clash TUN，由路由规则判定 DIRECT）。
`env -u http_proxy …` 只对当前这条命令清代理，不改全局设置；它设定的环境会被 `curl`、`install.sh` 及其内部再次发起的 `curl` 全部继承。

---

## 平台与产物

`install.sh` 按 `uname` 自动选产物，无需手动指定：

| 平台 | 产物 target | 备注 |
|------|-------------|------|
| macOS Apple Silicon | `aarch64-apple-darwin` | 本机 runner 314 构建 |
| macOS Intel | `x86_64-apple-darwin` | 交叉编译 |
| Linux x86_64 | `x86_64-unknown-linux-musl` | 静态产物，任意 glibc 可跑 |

其它平台报 `unsupported platform`，改用[源码编译](#源码编译-fallback)。

---

## 可选项

通过 flag / 环境变量调整（接在 `install.sh` 后面，或作为 `env` 变量传入）：

| 选项 | 作用 | 默认 |
|------|------|------|
| `--with-fuse` | 同时安装 `veda-fuse`（FUSE 挂载） | 否 |
| `VEDA_VERSION=0.1.16` | 锁定版本 | 自动取最新 |
| `VEDA_INSTALL_DIR=/path` | 安装目录 | root→`/usr/local/bin`，非 root→`$HOME/.local/bin` |
| `VEDA_SOURCE=gitlab\|github` | 产物来源 | `gitlab`（内网） |

`--with-fuse` 在 mac 上需先装 macFUSE：

```sh
brew install --cask macfuse
# 然后在「系统设置 → 隐私与安全性」授权系统扩展，再重跑 install.sh --with-fuse
```

---

## 安装后

- **PATH**：非 root 默认装到 `~/.local/bin`（多数 mac 已在 PATH）；root 装到 `/usr/local/bin`。
  若提示 `veda: command not found`，把目录加进 shell rc：
  ```sh
  echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.zshrc && source ~/.zshrc
  ```
- **skill**：安装器会顺带把 veda skill 写到 `~/.claude/skills/veda/SKILL.md`，Claude Code 可直接调用。
- **初始化**（首次使用，需要时再做）：
  ```sh
  veda init                       # 匿名零输入，立即可用
  veda init --email you@corp.com  # 具名账号
  ```
  server URL 已内置默认 `https://veda.ddmc-inc.com`，无需手动配。

---

## 故障排查

| 症状 | 原因 | 处理 |
|------|------|------|
| `SSL_ERROR_SYSCALL ... git.ddxq.mobi:443` | mac 代理劫持内网 | 加 `env -u http_proxy -u https_proxy -u HTTP_PROXY -u HTTPS_PROXY` 前缀 |
| `fetch failed ... LATEST_VERSION`（token expired） | 同上 / deploy token 失效 | 先清代理重试；仍失败用 `VEDA_VERSION=<x.y.z>` 跳过版本解析 |
| `download failed: veda-<target>` | 该版本产物未发布 | 确认版本号；或 `VEDA_SOURCE=github` 换源 |
| `unsupported platform` | 非 mac / linux-x64 | 走[源码编译](#源码编译-fallback) |
| 装完 `command not found` | 安装目录不在 PATH | 见上「安装后 → PATH」 |

---

## 源码编译 fallback

有 Rust 工具链、或拉不到预编译产物时，从本地仓库直接编：

```sh
cargo build --release -p veda-cli --bin veda
install -m 0755 target/release/veda ~/.local/bin/veda
veda --version
```

---

## 升级 / 卸载

```sh
# 升级：重跑 install.sh 即覆盖为最新版（同样记得清代理）
env -u http_proxy -u https_proxy -u HTTP_PROXY -u HTTPS_PROXY sh install.sh

# 卸载：删 binary（默认目录）
rm -f ~/.local/bin/veda
```
