# Veda 是什么

**Veda** 是一个可编程的知识存储服务：把文件、向量搜索、SQL 查询统一在一套 API 之下。一个 CLI、一个 HTTP 接口，背后是 MySQL（控制面）+ Milvus（数据面）+ 自动嵌入 worker。

可以理解为：**"自带搜索能力的网盘"** 加上 **"自动建索引的向量数据库"**，再加上 **"能直接跑 SQL 的查询引擎"**。

## 两种工作区类型

Veda 的 workspace 分两种，建库时选定，按场景挑：

| | 文件库 (File Workspace) | 向量库 (Vector Workspace) |
|---|---|---|
| **kind** | `fs` | `db` |
| **数据模型** | 文件 / 目录 | 向量记录（text + meta） |
| **接入方式** | CLI / FUSE / HTTP，`wk_` | REST API / SDK，数据面 `wk_`（控制面 `vk_`） |
| **典型场景** | 个人知识库、Agent 记忆、代码搜索 | 业务应用的托管向量检索（Pinecone 式） |
| **类比** | 自带搜索的网盘 | 托管向量数据库 |

下面**先讲文件库**（文件操作、混合搜索、结构化 collection、SQL、FUSE、摘要分层），**再讲向量库**（托管嵌入、混合检索、meta 过滤）。如果你是带着"给业务应用加向量检索"来的，直接跳到「向量库：能力与场景」一节。

## 四种使用形态

同一份数据，四种姿态消费，按场景选：

- **CLI** — `veda` 二进制，脚本和日常 shell 都用它
- **FUSE 挂载** — `veda-fuse` 把 workspace 挂成本地目录，**vim / VSCode / `make` / `rsync` 不感知背后是云存储**，写文件即同步上传、自动嵌入
- **MCP** — server 原生 `/mcp` 端点，Claude Code / Cursor 等 Coding Agent 在 `.mcp.json` 配一段 URL + `wk_` 即接入，零安装（见 [AI 助手集成](#/docs/skill)）
- **HTTP API** — REST + SSE 的 JSON 接口。前端、自建 agent、数据 pipeline 直接接入；向量库另有 Java SDK 与 Python 示例（见 [向量库 API](#/docs/vectors)）

---

## 文件库：核心能力

| 能力 | 说明 |
|---|---|
| **文件操作** | `cp` / `cat` / `ls` / `mv` / `rm` / `mkdir` / `append`，跟 Unix 一样的语义 |
| **混合搜索** | 每个文件自动分块、嵌入、入库。默认 hybrid（向量 + BM25 + RRF），也可单独走 semantic / fulltext |
| **结构化 collection** | 类似 vector-native 数据库：定义 schema + 自动嵌入字段，按字段过滤搜索 |
| **SQL 查询** | DataFusion 引擎跑在文件和 collection 之上，filter / aggregate / join 都能玩 |
| **多租户隔离** | Account → Workspace 两层；控制面账号 key `vk_`、数据面 workspace key `wk_` |
| **FUSE 挂载** | 把 workspace 挂成本地目录，用 vim / IDE / `make` 任意原生工具访问 |
| **摘要分层** | 每个文件自动生成 L0（一句话）和 L1（约 2k token 的概要），LLM 召回时省 token |
| **Agent 接入 / RAG 问答** | MCP 端点提供 6 个只读工具给 Coding Agent；`/v1/answer` / `veda ask` 返回带 [n] 引用的答案 |

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

- **Token 阶梯式投入**：100 个 L0 ≈ 10k token，100 个 L1 ≈ 200k token，100 个全文 ≈ MB 级。L0 → L1 → full 按需升级，不一上来 all-in。
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

## 文件库：典型场景

### 1. 个人知识库

笔记、技术文档、读书摘抄、代码 snippet 全塞进去，按语义召回（不是 grep 关键词）：

```bash
veda cp ~/Notes/2026-blockchain-paper.md /papers/blockchain-2026.md
veda cp ~/Notes/work /notes/work        # 目录自动递归上传（不需要 -r）
veda search "raft consensus 怎么处理 leader 切换"
```

### 2. AI Agent 的记忆 + 分布式状态

**a. 跨会话长期记忆** —— agent 把会话纪要存进去，下次开场用三层阶梯找回：

```bash
veda cp /tmp/session-2026-05-19.md /conversations/2026-05-19.md
veda search "上次说到的部署方案" --detail-level abstract
veda overview /conversations/2026-05-19.md     # 必要时升级
```

**b. 跨机器 / 跨实例的分布式状态** —— 多个 agent 实例共用一个 workspace，SSE 流秒级推送变更（服务端每秒轮询一次事件表）：

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
veda cp ~/work/internal-docs /knowledge/internal
veda search "我们的 retry 策略怎么定的" --detail-level abstract
```

配合内置的 [skill 系统](#/docs/skill)，Claude Code / Codex / Cursor 自动学会调用 `veda` CLI。

### 3. 跨多个代码库的搜索

```bash
veda cp ~/work/repo-a /code/repo-a      # 目录自动递归上传（不需要 -r）
veda cp ~/work/repo-b /code/repo-b
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

## 向量库：能力与场景

如果你要的不是"存文件"，而是**给业务应用加语义检索**，建 `kind=db` 的向量库。定位是 Pinecone 式托管向量检索，核心卖点一句话：**只写文本，不管向量**——嵌入模型、向量索引、BM25 全文索引全在服务端，业务方不需要自己维护 embedding 服务，也不用关心模型和维度。

| 能力 | 说明 |
|---|---|
| **托管嵌入** | 写入只给 `text`，服务端自动算向量 + 建全文索引；换嵌入模型是服务端的事，业务代码不动 |
| **三种检索模式** | `hybrid`（向量 + BM25 + RRF 融合，默认）/ `semantic`（纯语义，可设 `min_score` 相关度门槛）/ `fulltext`（纯 BM25，不依赖嵌入、最便宜） |
| **meta 过滤** | 每条记录可挂 `category` / `tags` / 自定义 `meta` JSON，检索时按 `meta.<key>` 做 eq / in / 范围过滤——"带过滤的 RAG"开箱即用 |
| **dataset 逻辑分组** | 一个库内按 `products` / `faq` 等 dataset 分组，互不串扰；省略时落 `default` |
| **两种写入模式** | 默认 `upsert` 幂等、重试安全；批量导入用 `insert` 跳过查重换 ~3x 吞吐（调用方保证 id 唯一） |
| **多租户隔离** | 一个库一个独立 Milvus collection；数据面凭证 `wk_` 按库签发、分 read / readwrite，撤一把 key 不影响其他接入方 |
| **接入方式** | REST API（curl 即通）+ Java SDK；建库、签 key 的控制面由平台 / 控制台代管，业务 app 拿到 `wk_` 就能跑 |

### 30 秒看懂用法

```bash
WK=wk_...   # 平台签发的数据面 key，绑定你的库

# 写入：只给文本和业务字段，服务端自动嵌入
curl -sX POST $BASE/v1/vectors/upsert \
  -H "authorization: Bearer $WK" -H 'content-type: application/json' \
  -d '{"records":[
        {"id":"sku-1","text":"Air Jordan 1 复刻款篮球鞋","meta":{"price":1299}},
        {"id":"sku-2","text":"Stan Smith 经典小白鞋","meta":{"price":499}}]}'

# 检索：语义 + 全文混合召回，带 meta 过滤
curl -sX POST $BASE/v1/vectors/search \
  -H "authorization: Bearer $WK" -H 'content-type: application/json' \
  -d '{"query":"一千五以内的篮球鞋","top_k":5,
       "filter":{"must":[{"field":"meta.price","op":"lt","value":1500}]}}'
```

### 典型场景

**1. 业务应用的语义搜索** —— 商品、FAQ、工单、内容库。"关键词搜不到"的查询变成语义召回：用户搜"一千五以内的篮球鞋"，BM25 兜住型号词、向量兜住意图，RRF 融合排序，meta 过滤收住价格区间。

**2. RAG 知识底座** —— 业务方自己切片、自己管内容，写入即可被检索。`semantic` + `min_score` 给召回设相关度门槛，避免低相关内容污染 prompt；预算敏感的链路可以走 `fulltext`（不调用嵌入，最便宜）。

**3. 相似内容 / 推荐** —— "看了又看"、相似工单归并、重复内容检测：拿当前条目的文本当 query 做 `semantic` 检索即可，无需单独的相似度服务。

**4. 多业务方向隔离** —— 一个账号多个库（或一个库多个 dataset），每个业务方向各拿一把 `wk_`；某个接入方下线或 key 泄露，撤那一把 key 即可，互不影响。

### 文件库还是向量库？

- 数据是**文件 / 文档**，人要用 CLI、编辑器直接读写，要目录结构、摘要、SQL → **文件库**
- 数据是**业务记录**（商品、FAQ 条目、内容切片），由应用程序写入和检索，要过滤、要吞吐 → **向量库**
- 两边都要？同一账号下两种库随便建，互不影响。

字段级契约（全部端点、限制、错误码、幂等语义）见 [向量库 API](#/docs/vectors)。

---

## 不擅长什么 / 边界

| 场景 | 限制 |
|---|---|
| 图片 / 视频 / 扫描件 | ❌ 不解析（OCR 未做）。文件库中 PDF / Word 会抽取文本可搜，其余二进制只存不索引；向量库仍只收文本 |
| 严格 ACL / 配额 | ❌ alpha 阶段没做细粒度权限和限额 |
| 高并发交易场景 | ❌ 是知识库 / 检索服务，不是 OLTP 数据库 |
| 海量小文件（>100 万 chunks） | ⚠️ alpha 单副本，规模化要等多副本演进 |
| 自带向量写入 | ❌ 向量库只收文本（托管嵌入），不接受业务方预先算好的向量 |
| 跨 dataset 检索 | ❌ 向量库单次 search 锁定一个 dataset，跨组检索需多次调用 |
| 超长单条文本 | ⚠️ 向量库单条 `text` 上限 64KB，更长内容需客户端先分片 |

---

## 接下来

- [**快速开始**](#/docs/quickstart) — 5 分钟从 onboard 到第一次搜索
- [**详细文档**](#/docs/reference) — 架构 / 认证 / 全部 API / 错误码 / 边界，一页查全
- [**向量库 API**](#/docs/vectors) — 面向业务方的托管向量检索
- [**CLI 速查**](#/docs/cli) — 全部命令在一页
- [**AI 助手集成**](#/docs/skill) — 把 Veda 接到 Claude Code / Cursor / Codex
- [**FUSE 挂载**](#/docs/fuse) — 本地目录形态
