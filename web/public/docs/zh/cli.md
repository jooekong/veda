# CLI 速查

权威参考是 `veda --help` 和 `veda <子命令> --help`。本页列最常用的。

> ⚠️ `veda` CLI 只服务**文件库**（`kind=fs`）。拿向量库（`kind=db`）的 `wk_` 跑任何数据命令都会返回 `400 WORKSPACE_KIND_MISMATCH`——裸 `veda status` 是唯一例外（它只 ping `/healthz`，所以看起来"能用"，但 `veda status --index` 照样 400）。向量库走 [向量库 API](#/docs/vectors)。

## 设置

```bash
# 用账号 key (vk_…) 连
veda init --server https://veda.ddmc-inc.com --import-key vk_xxx

# 或用 workspace key (wk_…)
veda init --server https://veda.ddmc-inc.com --import-key wk_xxx

# 直接用 CLI 注册带邮箱的账号
veda init --email you@example.com --password 'strong-pw'

# 已有账号登录（沿用账号原 api_key，并为默认 workspace 新签一把 wk_）
veda init --login --email you@example.com

# 匿名账号补邮箱/密码升级成具名账号（原 api_key 继续有效）
veda init --upgrade --email you@example.com
```

CI / agent 用非交互模式，密码走环境变量（`--password` 会出现在 `ps` 里）：

```bash
VEDA_PASSWORD='strong-pw' veda init --email you@example.com --non-interactive
```

不加 `--non-interactive` 时，具名 / 登录模式会先提示确认 Server URL；管道里没有 tty 会直接报错。已经初始化过的机器上再跑裸 `veda init` 会被拒绝（不是 bug，用 `veda workspace add` 或 `--import-key`）。

### workspace（本地 profile）

```bash
veda workspace add my-project                 # 新建 server workspace 并存为本地 alias
veda workspace add shared --workspace-id <id> # 给已有 workspace 签一把 key（跨机共享）
veda workspace list                           # 列本地 profile，★ 是当前活跃
veda workspace switch my-project              # 切换活跃 profile
veda workspace rm my-project                  # 只删本地 alias，不吊销服务端 wk_
veda ws list                                  # ws 是 workspace 的简写
```

临时切换：`veda --workspace archive ls /docs`——只对这一条命令生效，不改 config；alias 必须已存在。

配置在 `~/.config/veda/config.toml`（或目录级 `.veda.toml`，见下节）。`veda status` 看当前状态（server / 凭证来源 / 活跃 workspace）；`veda config show` 是隐藏的排错入口，能看到完整 profile 列表。

无 config 直连（CI / 脚本 / agent 场景，零落盘）：

```bash
export VEDA_SERVER=https://veda.ddmc-inc.com
export VEDA_KEY=wk_xxx
veda search "..."               # 数据面命令直接可用，不写任何本地文件
```

优先级 `--server` flag > 环境变量（`VEDA_SERVER` / `VEDA_KEY`）> config.toml。**key 没有 CLI flag**——只能走 `$VEDA_KEY` 或 config（`--key` 是 `veda-fuse mount` 独有的）。`veda status` 会标注凭证来源（env / config）。

### 目录级配置（`.veda.toml`）

把一份与 config.toml 同构的 `.veda.toml` 放进项目目录，veda 从当前目录**向上查找**，找到即**整体取代**全局配置（不做字段合并——局部文件永远借不到全局的 key）。这是给项目 / agent 绑定专属 workspace 的正规做法：

```toml
# .veda.toml — 含 wk_ key 时务必加进 .gitignore
server_url = "https://veda.ddmc-inc.com"
active_workspace = "default"

[workspaces.default]
key = "wk_xxx"
```

- 配置文件解析顺序：`$VEDA_CONFIG`（显式指定一个配置文件，**必须绝对路径**，与目录无关）> 就近 `.veda.toml` > 全局 config.toml。flag 和 `$VEDA_SERVER` / `$VEDA_KEY` 仍压过任何文件。
- 写回同源：目录配置生效时，`veda init` / `workspace switch` / `config set` 改的是 `.veda.toml`，不碰全局文件。
- `.veda.toml` 解析失败直接报错，**不会**静默回落全局（避免写进错误的 workspace）；文件存在但为空视为「本目录未配置」，同样不回落。
- `veda status` / `veda config show` 第一行显示当前生效的配置文件，带 `[local]` / `[$VEDA_CONFIG]` 标记。

团队 / agent 仓库推荐「**可提交、无密钥**」模式：`.veda.toml` 只写 `server_url`（不含 key，可以放心提交，worktree / 新 clone 天然带上），key 走 `$VEDA_KEY` 注入——wk_ key 本身就选定了 workspace。这样漏配 key 时得到的是明确报错，而不是静默打到全局配置。

注意：`$VEDA_KEY` 会发给就近 `.veda.toml` 指定的任何 server——设着它就不要在不可信的 checkout 里跑 veda；要彻底免疫，把 `$VEDA_SERVER` 也一并 export（env server 压过文件里的 server）。

## 文件系统

```bash
veda cp ./README.md /docs/readme.md          # 上传：本地 → 远端
veda cp ./src /code                          # 目录递归上传（src 是目录时自动 recursive）
veda cp ./repo /code --no-ignore             # 连 .gitignore 忽略的文件一起传
veda cp - /notes/scratch < input.txt         # 从 stdin 上传（src 写 "-"）
veda cat /docs/readme.md > ./readme.md       # 下载：远端 → 本地（用 cat 重定向，cp 只负责上传）
veda mv /old.md /archive/old.md
veda rm /tmp                                 # 删除（目录默认递归，没有 -r 参数；TTY 下有 y/N 确认）
veda rm /tmp /scratch/a.md                   # 可一次删多个；单个失败不中断，最后非 0 退出
veda mkdir /new-dir                          # 新建目录
veda append /notes/log "entry"               # 追加内容（也支持 "-" 从 stdin）

veda ls
veda ls /docs
veda ls /docs --json                         # 每行一个 JSON，jq 友好
veda cat /docs/readme.md
veda cat /docs/readme.md --range 10:20       # 1-indexed inclusive 行范围
veda cat /docs/readme.md --head 10           # 头 10 行
veda cat /docs/readme.md --tail 5            # 尾 5 行
veda cat /docs/design.pdf --raw > design.pdf # --raw 拿原始字节（PDF/Word 不加 --raw 输出的是提取文本）
```

文本与二进制都能 `cp` / `cat`（需 server ≥0.1.15）。PDF / Word 会自动抽取文本入索引可搜，`cat` 默认输出提取文本、`--raw` 拿原始字节；图片 / jar 等其余二进制只存不索引，`cat` 输出原始字节（重定向到文件）。

### 目录上传会跳过什么

`veda cp <目录>` 遵守**源目录树内**的 `.gitignore` 和 `.vedaignore`（同 gitignore 语法），外加一份内置兜底列表：`.git`、`__pycache__`、`.idea`、`node_modules`、`.DS_Store`。

这不是可有可无的过滤——**每个上传的文件都会消耗一次 embedding 调用和两次 LLM 摘要调用**。传一个 Rust 仓库而不跳 `target/`，几十万个构建产物会直接烧掉配额。

几条刻意的取舍：

- **dotfile 照传**。`.github/`、`.env.example`、`.cursor/rules` 都是真内容，不会因为以 `.` 开头就被丢掉。
- **不是 git 仓库也认 ignore 文件**。纯文档目录里放一个 `.vedaignore` 一样生效。
- **只看源目录树以内**。源目录**之上**的 `.gitignore`、你的全局 gitignore、`.git/info/exclude`、以及 `.ignore` 文件（ripgrep 约定）**都不读**——否则同一个目录在不同机器上会传出不同内容。
- `--no-ignore` 关掉 `.gitignore` / `.vedaignore`，但内置兜底列表仍然生效（`.git/` 任何情况下都不传）。

## 工作区布局

```bash
veda layout          # 顶层区域 + 每个区域的简短介绍 + 文件数
veda layout --json   # 结构化输出，脚本/agent 用
```

不认识一个 workspace 时的第一条命令：一次拿到全貌，省掉「`veda ls` 之后挨个 `veda abstract`」。只有顶层一层，想深入某个目录用 `veda overview <path>`。

每个条目是「一行标题 + 缩进的完整介绍」：

```
docs/  87 files
    veda 的项目文档区，收录架构说明、部署运维手册与设计方案。内容覆盖服务端与
    CLI 两侧，供开发和值班同学查阅。

tmp/  1 file
README.md  4.0 KB
    仓库入口说明，介绍 veda 是什么、怎么装、以及从哪里开始读文档。

213 files, 6 directories, 18 MB
```

介绍**不截断**，按终端宽度折行；输出重定向到管道时不折行，每条介绍保持一整行，方便 `grep`。没有摘要的条目只有标题行。

## 搜索

```bash
veda search "auth 是怎么做的"                       # hybrid（向量 + BM25 + RRF），默认
veda search "exact term" --mode fulltext
veda search "concept" --mode semantic
veda search "auth" --path /docs                    # 限定子树
veda search "auth" --limit 20
veda search "auth" --detail-level abstract         # 命中只返回 L0 摘要
veda grep "TODO(joe)" --limit 200                  # 字面匹配（同步，无 embedding 延迟），返回 file:line
veda grep "todo" /docs -i                          # 限定子树（位置参数）+ 忽略大小写
```

## 问答（RAG）

```bash
veda ask "这个系统怎么部署"            # 一站式回答，内联 [n] 引用 + 出处列表
veda ask "……" --path /docs             # 限定检索子树
veda ask "……" --json                   # 原始 JSON（直接是 data 对象，jq .citations 可解析）
```

服务端自主检索并生成带引用的答案，可能需要 10-90s。server 未配 LLM（501）和同 workspace 并发问答超限（429）各自打印一条可读中文提示，但**都以退出码 1 结束**——脚本要区分请匹配 stderr 文案，别看退出码。问题长度上限 1024 字符。

## 摘要分层

```bash
veda abstract /docs/readme.md   # L0 一句话
veda overview /docs/readme.md   # L1 ~2k token 概要
```

异步生成；未就绪返回 `Summary not ready yet`（exit 2），等几秒重试，或直接 `veda cat` 拿原文。server 未配 `[llm]` 时摘要功能整体关闭（exit 3）。这两个是 CLI 里**仅有**的自定义退出码。

## 结构化 collection

schema 用 JSON array 一次传入；`--embed-source` 指定自动嵌入的字段：

```bash
veda collection create articles \
  --schema '[{"name":"title","type":"string","index":true},
             {"name":"content","type":"string"},
             {"name":"category","type":"string","index":true}]' \
  --embed-source content

# 插入是 JSON 数组（不是单个对象）
veda collection insert articles '[
  {"title":"Intro to Rust","content":"...","category":"tech"},
  {"title":"Pasta","content":"...","category":"food"}
]'

veda collection list
veda collection desc articles
veda collection delete articles
veda collection search articles "systems programming" --limit 5

# 要做过滤 / 聚合走 SQL（collection search 不支持 --filter）
veda sql "SELECT title FROM articles WHERE category = 'tech' LIMIT 5"
veda sql "SELECT category, COUNT(*) FROM articles GROUP BY category"
```

## 杂项

```bash
veda status                     # 当前配置 + server 可达性（含凭证来源 env/config）
veda status --index             # 索引进度 {pending, processing, dead}
veda status --index --wait      # 轮询到全部可搜再退出；有永久失败则退出码非 0（可当 CI 门）
veda config show                # 配置详情
veda --version                  # 客户端版本
```
