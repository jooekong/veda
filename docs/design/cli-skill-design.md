# Veda CLI Skill 设计

**Date**: 2026-05-06
**Status**: Draft, awaiting review
**Author**: Joe Kong
**Target version**: 0.1.0

## 1. 背景与目标

让用户在 Claude Code（及任意 AI agent）里使用 veda 时，无需阅读 README、不用手动配置 server URL，一条命令完成「装 CLI → 配 server → 拿 API key → 开始用」全流程。

核心目标：
- **零摩擦安装**：`curl ... install.sh | sh` 一条命令搞定 CLI（可选加 `--with-fuse`）
- **AI agent 自动激活**：Claude Code 用户不用每次说"读那个 URL"，skill 在合适场景自动触发
- **跨 agent 可用**：skill.md 同时托管为 raw URL，Cursor/Cline/Codex 等 agent 也能 fetch 使用
- **半自动 auth**：agent 引导 user 自己跑 `veda account create`，避免邮箱密码进 chat history

非目标（v1 不做）：
- 公网 SaaS endpoint（先用内部域名 `https://veda.dbpaas.dingdongxiaoqu.com` 走 LB）
- 浏览器 SSO 登录
- 匿名账号自动注册（演进到 v2 再做）
- macOS GitLab Runner（darwin 半自动 release）
- 跨发行版 linux 包管理器集成（不出 .deb / .rpm）
- 向后兼容（alpha 期可自由打破）

## 2. 整体架构

```
┌─────────────────────────────────────────────────────────────────┐
│  Internal GitLab: git.ddxq.mobi/middleware/dbpaas/veda          │
│                                                                 │
│  Repo files                                                     │
│  ├─ install.sh           (raw URL，user 端入口)                 │
│  ├─ skill.md             (raw URL，agent 学习材料)              │
│  ├─ .gitlab-ci.yml       (tag 触发 release pipeline)            │
│  └─ scripts/release-darwin.sh (Joe 本地手动跑)                  │
│                                                                 │
│  Releases / Generic Packages                                    │
│  └─ veda/0.1.0/                                                 │
│      ├─ veda-x86_64-unknown-linux-gnu        + .sha256          │
│      ├─ veda-aarch64-unknown-linux-gnu       + .sha256          │
│      ├─ veda-x86_64-apple-darwin             + .sha256          │
│      ├─ veda-aarch64-apple-darwin            + .sha256          │
│      ├─ veda-fuse-x86_64-unknown-linux-gnu   + .sha256          │
│      ├─ veda-fuse-aarch64-unknown-linux-gnu  + .sha256          │
│      ├─ veda-fuse-x86_64-apple-darwin        + .sha256          │
│      └─ veda-fuse-aarch64-apple-darwin       + .sha256          │
└─────────────────────────────────────────────────────────────────┘
                            ▲                ▲
                CI runner upload│         Joe 本地 mac upload
                 (linux 4 个)   │         (darwin 4 个，via API)
                                │                │
                                └────────────────┘
                                        │
                                        │ user 端：
                                        │
                            curl -fL .../install.sh | sh
                                        │
                                        ▼
                        ┌──────────────────────────────┐
                        │ install.sh (deploy token 嵌) │
                        │ 1. 检测平台                  │
                        │ 2. [--with-fuse] 预检 FUSE 依│
                        │    赖；mac 提示，linux 可代装│
                        │ 3. 下载 binary + sha256 校验 │
                        │ 4. 装到 ~/.local/bin/         │
                        │ 5. 复制 skill.md 到          │
                        │    ~/.claude/skills/veda/    │
                        │    SKILL.md（仅当目录存在）  │
                        │ 6. 提示 PATH + 下一步        │
                        └──────────────────────────────┘
                                        │
                                        ▼
                        ┌──────────────────────────────┐
                        │ AI Agent（Claude Code 等）   │
                        │ - 读 skill.md 学命令         │
                        │ - 引导 user 配 server + key  │
                        │ - 跑 veda 命令完成任务       │
                        └──────────────────────────────┘
```

## 3. 关键决策摘要

| 决策点 | 结论 | 备注 |
|--------|------|------|
| 部署模式 | SaaS 默认（目标态） | 当前 endpoint 用内部域名 `https://veda.dbpaas.dingdongxiaoqu.com` 走 LB |
| Auth UX | 手动注册 + 粘贴 key（C） | v2 演进到匿名优先（A） |
| 二进制分发 | GitLab Generic Packages + release-cli | 内部 GitLab，deploy token 嵌入 install.sh |
| Mac 架构 | 双发（x86_64 + aarch64 darwin） | Joe 本地 build，Intel 同事仍多 |
| Linux 架构 | x86_64 + aarch64 | CI cargo-zigbuild 交叉 |
| FUSE 范围 | v1 包含 | install.sh `--with-fuse` 选装 |
| FUSE 依赖 | mac 提示安装；linux 可代跑 sudo | 不强制接管 |
| Skill 形态 | 双轨：hosted URL + ~/.claude/skills/veda/ | Claude Code 自动激活 + 跨 agent |
| 内容范围 | 全套（文件/搜索/collection/SQL/FUSE） | 不做 minimal |

## 4. Release Pipeline

### 4.1 制品矩阵

每个 release 产出 8 个二进制 + 8 个 sha256 + 1 份 skill.md：

| Asset | Targets |
|--------|---------|
| `veda` binary | x86_64-linux, aarch64-linux, x86_64-darwin, aarch64-darwin |
| `veda-fuse` binary | 同上 |
| `skill.md` | 单一文件（agent 学习材料，install.sh 用 deploy token 拉它） |

### 4.2 GitLab CI（linux 部分）

`.gitlab-ci.yml` 在 tag 推送时触发，跑在 linux runner。

```yaml
stages: [build, publish, release]

variables:
  REGISTRY: "${CI_API_V4_URL}/projects/${CI_PROJECT_ID}/packages/generic/veda/${CI_COMMIT_TAG}"

build:linux:
  stage: build
  image: rust:1.83
  rules:
    - if: $CI_COMMIT_TAG
  parallel:
    matrix:
      - TARGET: [x86_64-unknown-linux-gnu, aarch64-unknown-linux-gnu]
  before_script:
    - apt-get update && apt-get install -y curl xz-utils libfuse3-dev pkg-config
    - curl -fsSL https://ziglang.org/download/0.13.0/zig-linux-x86_64-0.13.0.tar.xz | tar xJ -C /opt
    - export PATH=/opt/zig-linux-x86_64-0.13.0:$PATH
    - cargo install --locked cargo-zigbuild
    - rustup target add ${TARGET}
  script:
    - cargo zigbuild --release --target ${TARGET} --bin veda
    - cargo zigbuild --release --target ${TARGET} --bin veda-fuse -p veda-fuse
    - mv target/${TARGET}/release/veda      veda-${TARGET}
    - mv target/${TARGET}/release/veda-fuse veda-fuse-${TARGET}
    - sha256sum veda-${TARGET}      > veda-${TARGET}.sha256
    - sha256sum veda-fuse-${TARGET} > veda-fuse-${TARGET}.sha256
  artifacts:
    paths: [veda-*, veda-fuse-*]
    expire_in: 1 week

publish:linux:
  stage: publish
  image: curlimages/curl:8.5.0
  rules:
    - if: $CI_COMMIT_TAG
  needs: ["build:linux"]
  script:
    - |
      for f in veda-* veda-fuse-* skill.md; do
        curl --fail --header "JOB-TOKEN: $CI_JOB_TOKEN" \
             --upload-file "$f" \
             "${REGISTRY}/$f"
      done

release:create:
  stage: release
  image: registry.gitlab.com/gitlab-org/release-cli:latest
  rules:
    - if: $CI_COMMIT_TAG
  needs: ["publish:linux"]
  # darwin 资产由 release-darwin.sh 单独 append；这里只为 linux 资产建 release 骨架
  script:
    - |
      release-cli create --name "Veda ${CI_COMMIT_TAG}" --tag-name "${CI_COMMIT_TAG}" \
        $(for f in veda-x86_64-unknown-linux-gnu veda-aarch64-unknown-linux-gnu \
                   veda-fuse-x86_64-unknown-linux-gnu veda-fuse-aarch64-unknown-linux-gnu; do \
            echo "--assets-link {\"name\":\"$f\",\"url\":\"${REGISTRY}/$f\",\"link_type\":\"package\"}"; \
          done)
```

### 4.3 darwin 半自动 release

`scripts/release-darwin.sh`（Joe 在自己 mac 上 tag 推完后执行）：

```bash
#!/bin/bash
set -euo pipefail

VERSION="$1"  # e.g. 0.1.0
TOKEN="${GITLAB_TOKEN:?需要 PAT，scope=api}"
PROJECT_ID="${VEDA_PROJECT_ID:?GitLab project ID}"
REGISTRY="https://git.ddxq.mobi/api/v4/projects/${PROJECT_ID}/packages/generic/veda/${VERSION}"

# 安装目标（一次性）：rustup target add x86_64-apple-darwin aarch64-apple-darwin

for TARGET in aarch64-apple-darwin x86_64-apple-darwin; do
  for BIN in veda veda-fuse; do
    PKG=""
    [ "$BIN" = "veda-fuse" ] && PKG="-p veda-fuse"
    cargo build --release --target "$TARGET" --bin "$BIN" $PKG
    cp "target/${TARGET}/release/${BIN}" "${BIN}-${TARGET}"
    shasum -a 256 "${BIN}-${TARGET}" > "${BIN}-${TARGET}.sha256"

    # upload binary + sha256
    for f in "${BIN}-${TARGET}" "${BIN}-${TARGET}.sha256"; do
      curl --fail --header "PRIVATE-TOKEN: ${TOKEN}" \
           --upload-file "$f" "${REGISTRY}/$f"
    done

    # append release asset link
    curl --fail --request POST \
         --header "PRIVATE-TOKEN: ${TOKEN}" \
         --header "Content-Type: application/json" \
         --data "{\"name\":\"${BIN}-${TARGET}\",\"url\":\"${REGISTRY}/${BIN}-${TARGET}\",\"link_type\":\"package\"}" \
         "https://git.ddxq.mobi/api/v4/projects/${PROJECT_ID}/releases/${VERSION}/assets/links"
  done
done
```

**前置条件**（一次性）：
- 安装 macFUSE（`brew install --cask macfuse` + 授权 system extension）—— veda-fuse 编译需要 SDK
- `rustup target add x86_64-apple-darwin aarch64-apple-darwin`
- 创建 GitLab PAT（scope: `api`），导出 `GITLAB_TOKEN`

### 4.4 演进路径

- v1.x：linux 全自动，darwin 手动
- v2.x：搞一台 mac mini 当 GitLab Runner，把 darwin 也并到 CI 里

## 5. install.sh 设计

### 5.1 入口

```sh
# 默认仅装 CLI
curl -fL https://git.ddxq.mobi/middleware/dbpaas/veda/-/raw/main/install.sh | sh

# 加装 veda-fuse
curl -fL https://git.ddxq.mobi/middleware/dbpaas/veda/-/raw/main/install.sh | sh -s -- --with-fuse

# 自定义安装目录
VEDA_INSTALL_DIR=$HOME/bin curl -fL ... | sh
```

### 5.2 行为分步

CLI 永远先装好（即使 `--with-fuse` 后续步骤失败，user 也至少有 CLI 可用）：

```
1. parse args: --with-fuse (bool), --version (default: 0.1.0)

2. detect platform
   uname -s/-m → TARGET 字符串
   不支持组合 → exit 1

3. 下载并安装 veda CLI（永远执行）
   DEPLOY_TOKEN="<deploy-token-value>"  # 嵌入到脚本中（scope=read_package_registry，限本 project）
   PROJECT_ID="9462"
   REG="https://git.ddxq.mobi/api/v4/projects/${PROJECT_ID}/packages/generic/veda/${VERSION}"
   curl --fail --header "Deploy-Token: ${DEPLOY_TOKEN}" \
        -L "${REG}/veda-${TARGET}"        -o "/tmp/veda-${TARGET}"
   curl --fail --header "Deploy-Token: ${DEPLOY_TOKEN}" \
        -L "${REG}/veda-${TARGET}.sha256" -o "/tmp/veda-${TARGET}.sha256"
   (cd /tmp && shasum -a 256 -c "veda-${TARGET}.sha256") || exit 1
   DEST="${VEDA_INSTALL_DIR:-$HOME/.local/bin}"
   mkdir -p "$DEST"
   mv "/tmp/veda-${TARGET}" "$DEST/veda"; chmod +x "$DEST/veda"

4. [--with-fuse] FUSE 依赖预检（CLI 已装好，这步失败不影响 CLI）
   macOS:
     test -e /Library/Filesystems/macfuse.fs
     缺 → echo "veda CLI 已装好。但 macFUSE 未安装，跳过 veda-fuse。"
          echo "请运行：brew install --cask macfuse"
          echo "首次安装后需在 系统设置 → 隐私与安全性 中授权 system extension"
          echo "之后重跑 install.sh --with-fuse"
          exit 0  # CLI 装好了，正常退出
   Linux:
     ldconfig -p | grep -q libfuse3 || dpkg -l libfuse3-3 2>/dev/null | grep -q ^ii
     缺 → 探测 /etc/os-release 的 ID
       debian|ubuntu: CMD="sudo apt-get install -y libfuse3-3 fuse3"
       fedora|rhel|centos: CMD="sudo dnf install -y fuse3"
       arch: CMD="sudo pacman -S --noconfirm fuse3"
       *: 仅打印命令，exit 0
       prompt user "是否运行 $CMD ？[y/N]"
       y → eval $CMD || { echo "FUSE 装失败，CLI 已就绪"; exit 0; }
       n → echo "跳过 FUSE，CLI 已就绪"; exit 0

5. [--with-fuse] 下载 veda-fuse
   curl + sha256 校验 + mv 到 $DEST/veda-fuse + chmod +x

6. PATH 检查
   case ":$PATH:" in
     *":$DEST:"*) ;;
     *) echo "提示：将 $DEST 加到 PATH，例如：echo 'export PATH=\"$DEST:\$PATH\"' >> ~/.zshrc" ;;
   esac

7. 复制 skill 到 Claude Code（不影响主流程，失败仅警告）
   if [ -d "$HOME/.claude" ]; then
     mkdir -p "$HOME/.claude/skills/veda"
     # raw 文件走 Deploy-Token 拉不到（deploy token 不带 read_repository scope）
     # 把 skill.md 也作为 release asset 上传，用 Deploy-Token 走 packages 接口拉
     curl --fail --header "Deploy-Token: ${DEPLOY_TOKEN}" \
          -L "${REG}/skill.md" \
          -o "$HOME/.claude/skills/veda/SKILL.md" \
       || echo "warn: skill.md 下载失败，可手工 fetch"
   fi

8. 打印下一步
   echo "✓ veda 已安装"
   echo "下一步："
   echo "  veda config set server_url https://veda.dbpaas.dingdongxiaoqu.com"
   echo "  veda account create --name <你> --email <邮箱> --password <密码>"
   echo "  veda config set api_key <上一步输出的 key>"
   echo "  veda workspace create my-workspace && veda use my-workspace"
   echo
   echo "Claude Code 用户：skill 已自动安装到 ~/.claude/skills/veda/"
   echo "其他 agent：让 agent 读取 https://git.ddxq.mobi/.../-/raw/main/skill.md"
```

### 5.3 安全考量

- **deploy token 嵌入**：read-only，scope 限到本 project 的 packages/release_artifacts。即使被人 cat 出来，也只能下载本仓库的 release，无写权限
- **sha256 校验**：每个 binary 必校验，防止下载中断或中间人篡改
- **install.sh 自身完整性**：建议 user 加 `curl -fL ... | sh` 改为 `curl -fL ... -o install.sh; less install.sh; sh install.sh`，但默认接受 pipe-to-shell 的便利性
- **PATH 不主动改**：只提示，不写 user 的 rc 文件

## 6. Skill 形态

### 6.1 双轨分发

- **Canonical source**: `https://git.ddxq.mobi/middleware/dbpaas/veda/-/raw/main/skill.md`
- **Claude Code 副本**: `~/.claude/skills/veda/SKILL.md`（install.sh 复制）

更新策略：
- 改 skill.md 推到 main 分支立即在 raw URL 生效
- 已安装用户的本地副本不会自动更新——v1 接受这个限制；v2 加一个 `veda skill update` 子命令

### 6.2 SKILL.md frontmatter

```yaml
---
name: veda
description: |
  Knowledge store with file ops, semantic+fulltext search, structured collections,
  and SQL queries. Use when user mentions veda, wants to upload/cat/list/search files
  in their personal knowledge base, manage structured collections, run SQL across
  their data, or mount their workspace as a local filesystem (FUSE).
---
```

`description` 决定 Claude Code 何时自动激活。覆盖关键词：veda、knowledge base、semantic search、upload files、collection、SQL、FUSE mount。

### 6.3 内容章节大纲

```
# Using Veda

## What Veda is
1 段简介：file system + vector search + SQL，多租户。所有操作通过 `veda` CLI。

## Bootstrap (first-time setup)
[详见 §7 首次使用流]

## Core operations
### File operations
- veda cp <src> <dst>      上传 / 拷贝
- veda ls <path>            列目录
- veda cat <path> [--lines] 读文件（支持行范围）
- veda mv / rm / mkdir      标准操作
- veda append <path> <content>  追加内容（或 stdin "-"）

### Search
- veda search "query"                          默认 hybrid（vector + BM25 + RRF 融合）
- veda search "query" --mode semantic          仅向量
- veda search "query" --mode fulltext          仅 BM25
- veda search "query" --detail-level abstract  返回摘要而非完整内容（省 token）

### Summary
- veda summary <path>      看 L0/L1 自动摘要

### Structured collections
- veda collection create <name> --field "name:type:flag" ...
  - type: string | int | float | bool
  - flag: index（可过滤）/ embed（自动 embed 该字段）
- veda collection insert <name> --data '{...}'
- veda collection search <name> "query" --filter "..." --limit N
- veda collection create <name> --dim 768 --type raw   # 原始向量

### SQL
- veda sql "SELECT ... FROM <collection>"      DataFusion 引擎，标准 SQL

### FUSE mount (--with-fuse only)
- veda-fuse mount /mnt/veda --server $URL --key $KEY
- veda-fuse umount /mnt/veda
何时用：grep/ripgrep 大量文件、本地编辑器打开 veda 内容

## Decision rules (when to use what)
- "上传 / 把这个文件放到 veda" → veda cp
- "查找 / 搜索 / 找以前写的 X" → veda search（默认 hybrid）
- 用户要精确字符串（变量名、UUID、TODO）→ --mode fulltext
- 表格数据带 schema、需要 filter → collection（不是 file）
- 跨集合聚合查询 → veda sql
- 用户要 grep / 用本地工具浏览 → 提示装 FUSE

## Error handling
| 错误信息 | 含义 | agent 应对 |
|---------|------|-----------|
| no API key configured | 没 auth | 引导 user 跑 veda account create / login |
| no workspace selected | 没选 ws | veda workspace list → veda use <name> |
| connection refused | server 不通 | 问 user 当前 server URL；veda config show 检查 |
| 401 unauthorized | key 失效 | 让 user 重新 veda account login |
| 4xx with JSON body | API 错误 | 把 server 返回的 message 转告 user |

## Don't do
- 不要往 veda 塞大二进制（PDF/img 只存提取文本，原文件丢弃）
- 不要用 search 做精确路径查找——用 ls / cat
- 不要写到 :/ 根目录，挑个语义目录（如 :/notes/, :/code/）
- 不要在 chat 里把 user 的密码/API key 复述出来

## Reference
- GitLab: https://git.ddxq.mobi/middleware/dbpaas/veda
- Server: https://veda.dbpaas.dingdongxiaoqu.com
- Issues: 找 Joe
```

## 7. 首次使用 UX

```
User: 我想用 veda 把这些 markdown 都搜索起来

Agent (skill 触发):
  Step 1: 跑 `which veda`
    没有 → 说 "你还没装 veda，我帮你装。需要管理员权限的话会提前告诉你"
           跑 curl -fL ... install.sh | sh
    有 → 跳到 Step 3

  Step 2: 装好后，跑 veda config show
    server_url 仍是 default localhost:3000 → 跑 veda config set server_url https://veda.dbpaas.dingdongxiaoqu.com

  Step 3: 检查 api_key
    没有 → 对 user 说：
       "需要 API key。请你自己运行下面的命令（密码不要告诉我）：
          veda account create --name <你的名字> --email <你的邮箱> --password <你的密码>
        命令会输出一段 api_key，把它贴回来给我"
    user 贴回来 → 跑 veda config set api_key <key>
    （key 进 ~/.config/veda/config.toml，权限 0600，不会进 chat history 之外）

  Step 4: workspace
    跑 veda workspace list
      为空 → 问 user "用什么名字建 workspace？" → veda workspace create + veda use
      有 → 让 user 选

  Step 5: 验证
    跑 veda ls /，不报错就 OK

  Step 6: 执行用户原始任务
    veda cp ./*.md :/notes/
    veda search "原始问题里的关键词"
```

## 8. 演进路径

| 版本 | 变化 | 触发条件 |
|------|------|---------|
| 0.1.x | 当前 spec | alpha 期 |
| 0.2.0 | endpoint 切到 veda.ai 公网域名；deploy token 替换为公开 release | SaaS 上线 |
| 0.3.0 | 浏览器 SSO（`veda login` 起本地 callback） | OAuth 后端就绪 |
| 0.4.0 | 匿名优先 auth（`veda init` 自动建匿名账号） | 配额机制就绪 |
| 1.0.0 | macOS GitLab Runner，darwin 全自动 release | 拿到 mac mini |
| 1.x   | `veda skill update` 子命令同步本地 SKILL.md | 用户基数变大、内容更新频繁 |

## 9. 开放问题与决策

### 已决策
1. **GitLab project ID = 9462**（已确认）
2. **`veda config init` 不新增**：skill 引导 agent 直接调多个 `veda config set <key> <value>`。优点：CLI 零改动；缺点：skill 章节稍长，可接受
3. **macOS 双架构 v1 必发**：Joe 自己就是 Intel mac 用户；alpha 期内不砍 x86_64-darwin
4. **Skill 自更新**：install.sh 每次执行都覆盖 `~/.claude/skills/veda/SKILL.md`；不做独立的 `veda skill update`
5. **Skill.md 分发渠道（修订）**：deploy token 默认 scope `read_package_registry` 不能拉 raw repo 文件。所以 release pipeline 把 `skill.md` 也作为 generic package asset 上传，install.sh 走 packages 接口拉。canonical "raw URL" 给非 install.sh 用户走（公开 visibility 的话能直接 fetch；private 的话他们手动用 PAT）

### 待 Joe 在 GitLab UI 操作
6. **创建 Deploy Token**（项目级，read-only）
   - 路径：项目 → Settings → Repository → 展开 "Deploy tokens"
   - Name: `veda-installer`
   - Expiration: 留空或 1 年（alpha 阶段可重发）
   - Username: 留空（GitLab 自动生成 `gitlab+deploy-token-NNN`）
   - Scopes: 仅勾 `read_package_registry`（**不要**勾 read_repository / write_*）
   - 点 "Create deploy token"，**立即复制 token 值**（只显示一次）
   - 把值填到 install.sh 的 `DEPLOY_TOKEN=`

7. **确认 GitLab Runner 可用性**
   - 路径：项目 → Settings → CI/CD → 展开 "Runners"
   - 看到 **Project runners** 或 **Shared runners**（绿色 online 标记）即可用
   - 实际验证：推一个测试 tag（例如 `v0.0.1-test`）看 pipeline 是否真的跑起来
   - 如果完全没 runner：找公司平台组要一个 shared runner 接入，或自己起一个 docker runner（`gitlab-runner register` + linux 节点）

## 10. 文件清单（实施时新增）

```
veda/
├── install.sh                       # NEW，repo root，curl 入口
├── skill.md                         # NEW，repo root，hosted skill 内容
├── .gitlab-ci.yml                   # NEW，release pipeline
└── scripts/
    └── release-darwin.sh            # NEW，Joe 本地 mac 跑
```

无现有源码改动；veda-cli 和 veda-fuse 保持不变。

## 11. 测试计划

- **单元**：install.sh 各分支用 shellcheck + bats 跑（platform 检测、FUSE 预检、PATH 提示）
- **CI 集成**：在 GitLab CI 里起一个 `test:install` job，用 `docker run -v $PWD:/work ubuntu:22.04 sh /work/install.sh` 跑装一遍（用 mocked release URL）
- **手工 e2e**：每次 release 前在三种 fresh 环境跑：
  1. macOS arm64（Joe 的 mac，新建 user）
  2. linux x86_64（公司 dev 机）
  3. linux aarch64（如果有 ARM 服务器，否则跳）
  四步：装 → 配 → cp 一个文件 → search 它
- **Skill 行为验证**：在 fresh Claude Code 会话里说 "我要用 veda 搜索我的笔记"，验证 skill 自动激活、走完 §7 的 UX 流
