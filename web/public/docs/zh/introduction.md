# Veda 是什么

**Veda** 是一个可编程的知识存储服务：把文件、向量搜索、SQL 查询统一在一套 API 之下。一个 CLI、一个 HTTP 接口，背后是 MySQL（控制面）+ Milvus（数据面）+ 自动嵌入 worker。

可以理解为：**"自带搜索能力的网盘"** 加上 **"自动建索引的向量数据库"**，再加上 **"能直接跑 SQL 的查询引擎"**。

## 三种使用形态

同一份数据，三种姿态消费，按场景选：

- **CLI** — `veda` 二进制，脚本和日常 shell 都用它
- **FUSE 挂载** — `veda-fuse` 把 workspace 挂成本地目录，**vim / VSCode / `make` / `rsync` 不感知背后是云存储**，写文件即同步上传、自动嵌入
- **HTTP API** — REST + SSE，OpenAPI 风格的 JSON 接口。前端、自建 agent、数据 pipeline 直接接入；官方 SDK（Python / TypeScript / Go）即将推出，进一步降低集成成本

---

## 核心能力

| 能力 | 说明 |
|---|---|
| **文件操作** | `cp` / `cat` / `ls` / `mv` / `rm` / `mkdir` / `append`，跟 Unix 一样的语义 |
| **混合搜索** | 每个文件自动分块、嵌入、入库。默认 hybrid（向量 + BM25 + RRF），也可单独走 semantic / fulltext |
| **结构化 collection** | 类似 vector-native 数据库：定义 schema + 自动嵌入字段，按字段过滤搜索 |
| **SQL 查询** | DataFusion 引擎跑在文件和 collection 之上，filter / aggregate / join 都能玩 |
| **多租户隔离** | Account → Workspace 两层；API key 或 workspace key 鉴权 |
| **FUSE 挂载** | 把 workspace 挂成本地目录，用 vim / IDE / `make` 任意原生工具访问 |
| **摘要分层** | 每个文件自动生成 L0（一句话）和 L1（约 2k token 的概要），LLM 召回时省 token |

---

## 每个文件 / 目录的三层视图

| 层 | 命令 | 大小 | 文件含义 | 目录含义 |
|---|---|---|---|---|
| **abstract** (L0) | `veda abstract /path` | 一句话 | 一句话概括文件内容 | 一句话概括目录下大致是什么 |
| **overview** (L1) | `veda overview /path` | ~2k token | 结构化概要（章节 / 论点 / 关键数据） | 子目录与文件组成的层级摘要 |
| **full** | `veda cat /path` 或 `veda ls /path` | 全文 / 列表 | 原文 | 子项列表 |

```bash
veda abstract /docs/readme.md      # 文件 L0
veda abstract /knowledge/auth      # 目录 L0 ← 整个子树一句话
veda overview /knowledge/auth      # 目录 L1 ← 整个子树结构化摘要
veda cat /docs/readme.md           # 原文
```

### 为什么这是收益

- **Token 阶梯式投入**：100 个 L0 ≈ 5k token，100 个 L1 ≈ 200k token，100 个全文 ≈ MB 级。L0 → L1 → full 按需升级，不一上来 all-in。
- **目录探索几乎零成本**：`veda abstract /knowledge/internal/auth` 一句话就告诉你这个子树大概是什么，省掉 `ls && cat` 一圈。
- **服务端一次生成，多端共享**：摘要预计算，CLI / FUSE / 自建 agent 读同一份，不会出现"两个 agent 用不同 prompt 给出不一致摘要"。
- **模型自己决定深度**：RAG 不再固定 top-k 截断，在 L0 命中里 agent 自己挑哪些升 L1、哪些一句话就够。

### 在 search / FUSE 里直接用

Search 暴露这套阶梯：

```bash
veda search "deployment 怎么做" --detail-level abstract    # 命中只返回 L0
veda search "..." --detail-level overview                  # 命中返回 L1
veda search "..." --detail-level full                      # 命中返回原文（默认）
```

FUSE 挂载时，摘要以 sidecar 文件直接出现在每个目录下：

```bash
cat /mnt/veda/docs/.abstract       # 当前目录的 L0
cat /mnt/veda/docs/.overview       # 当前目录的 L1
```

> 摘要跟 embedding 在同一异步链路上生成；刚写入的文件 / 目录可能返回 `Summary not ready yet`，几秒后重试。
> workspace 根 `/` 本身不能 abstract（server 没有根 dentry），从任一子目录开始即可。

---

## 典型使用场景

### 1. 个人知识库

笔记、技术文档、读书摘抄、代码 snippet 全塞进去，按语义召回（不是 grep 关键词）：

```bash
veda cp ~/Notes/2026-blockchain-paper.md /papers/blockchain-2026.md
veda cp -r ~/Notes/work /notes/work
veda search "raft consensus 怎么处理 leader 切换"
```

### 2. AI Agent 的记忆 + 分布式状态

**a. 跨会话长期记忆** —— agent 把会话纪要存进去，下次开场用三层阶梯找回：

```bash
veda cp /tmp/session-2026-05-19.md /conversations/2026-05-19.md
veda search "上次说到的部署方案" --detail-level abstract
veda overview /conversations/2026-05-19.md     # 必要时升级
```

**b. 跨机器 / 跨实例的分布式状态** —— 多个 agent 实例共用一个 workspace，SSE 流自动推送变更（~120s 内）：

```bash
veda cp /tmp/todo.json /state/agent-todo.json   # 实例 A 写
veda cat /state/agent-todo.json                 # 实例 B 读
```

**c. 长任务的检查点 & 恢复** —— 每步写检查点，崩了接着跑：

```bash
veda cp /tmp/step-12-result.json /checkpoints/job-X/step-12.json
veda ls /checkpoints/job-X
```

**d. 多 agent 协作** —— planner / coder / reviewer 各自往子目录写产物，下游 search 取上游：

```bash
veda cp plan.md /agents/planner/2026-05-19-plan.md
veda search "deployment plan" --path /agents/planner --limit 1
```

**e. 预热知识库（RAG）** —— 代码库 / 文档预先嵌入，agent 启动零延迟召回：

```bash
veda cp -r ~/work/internal-docs /knowledge/internal
veda search "我们的 retry 策略怎么定的" --detail-level abstract
```

配合内置的 [skill 系统](#/docs/skill)，Claude Code / Codex / Cursor 自动学会调用 `veda` CLI。

### 3. 跨多个代码库的搜索

```bash
veda cp -r ~/work/repo-a /code/repo-a
veda cp -r ~/work/repo-b /code/repo-b
veda search "如何处理 retry" --path /code
veda grep "TODO(joe)" --limit 200      # 字面 grep，同步 + 行号
```

`grep` 同步精确匹配查 identifier；`search` 异步语义召回查概念。

### 4. 结构化数据 + 向量

带过滤的 RAG。完整 schema + 命令见 [CLI 速查 — 结构化 collection](#/docs/cli)；要点：`content` 字段自动嵌入，`title` / `category` 做过滤索引，过滤 / 聚合走 `veda sql`。

### 5. 团队共享上下文

一个 workspace 多把 key 分发：

- 同事各自拿 `wk_readwrite` 写笔记
- CI/CD 拿 `wk_read` 只读
- 撤销单把 key 不影响账号

### 6. 把 workspace 挂成本地目录

`vim` / VSCode / `make` 直接读写远端 workspace，详见 [FUSE 挂载](#/docs/fuse)。

---

## 不擅长什么 / 边界

| 场景 | 限制 |
|---|---|
| 二进制大文件（PDF / 图片 / 视频） | ❌ 目前只支持 UTF-8 文本 |
| 严格 ACL / 配额 | ❌ alpha 阶段没做细粒度权限和限额 |
| 高并发交易场景 | ❌ 是知识库不是 OLTP 数据库 |
| 海量小文件（>100 万 chunks） | ⚠️ alpha 单副本，规模化要等多副本演进 |

---

## 接下来

- [**快速开始**](#/docs/quickstart) — 5 分钟从 onboard 到第一次搜索
- [**CLI 速查**](#/docs/cli) — 全部命令在一页
- [**AI 助手集成**](#/docs/skill) — 把 Veda 接到 Claude Code / Cursor / Codex
- [**FUSE 挂载**](#/docs/fuse) — 本地目录形态
