# Veda

[English](README.md) | 简体中文

一个可编程的知识存储服务，把文件系统、向量搜索和 SQL 统一在一起——一个服务端、一套 API、一个 CLI。写入一个文件，它就能按语义被搜到；建一个 collection，每行数据自动 embedding；SQL 可以直接查询这一切。

## 解决什么问题

做任何检索密集型应用（AI agent 的记忆、RAG 后端、内部语义搜索），每次都要拼同一套技术栈：对象存储放文档、自己写并维护 chunking + embedding 的 ETL 流水线、一个向量数据库、一个全文引擎（纯向量搜不准标识符和精确词）、一个关系库放结构化数据，还有多租户的 key 管理。每条接缝的一致性都得自己兜着——文档改了，没人替你重新 embedding，除非你自己实现了这套联动。

Veda 把这条流水线收进一个服务：

```bash
veda cp ./design.pdf /docs/design.pdf        # 存储（文本或二进制；PDF/Word 自动抽取文本）
veda search "为什么选 outbox 模式"            # 几秒后即可 hybrid（语义 + BM25）搜索
veda sql "SELECT path, size_bytes FROM files WHERE path LIKE '/docs/%'"
```

Chunking、embedding、索引、摘要全部在服务端异步完成。文件存储与向量索引之间的一致性是服务端的责任而不是你的：文件写入和它的同步任务在同一个 MySQL 事务里提交（outbox 模式），后台 worker 负责让 Milvus 追上。

## 适用场景

- **AI agent 记忆 / 知识库** — Coding Agent（Claude Code / Cursor 等）通过 server 原生 MCP 端点一段 `.mcp.json` 即接入，或走 `veda` CLI（安装器自带面向 agent 的 `skill.md`）。三层摘要（L0 一句话 → L1 概览 → 全文）让 agent 在不读全文、不烧 token 的前提下快速筛选大量文件。
- **RAG 后端** — 上传文档（Markdown、源代码、PDF/Word），直接获得带相关性分数的 chunk 级 hybrid 搜索，或经 `/v1/answer` / `veda ask` 拿带内联引用的合成答案，不用单独运维一套 ETL。
- **自托管向量数据库** — `kind=db` workspace 提供 Pinecone 风格的裸向量数据面（upsert/search/query/delete + 元数据过滤），适合只需要向量的应用，另有 Java SDK。
- **能按语义 grep 的文件系统** — 用 FUSE 把 workspace 挂载成本地目录，vim/IDE 直接编辑，所有内容保持语义索引；字面匹配用 `veda grep`，概念搜索用 `veda search`。
- **平台构建块** — 面向网关的 surface（`/v1/workspace/{workspace}/project/...`）让 AI 平台把 veda 嵌入为自己的存储层，鉴权外置给平台网关。

## 功能

- **文件系统** — `cp`、`cat`、`ls`、`mv`、`rm`、`append`、`mkdir`，路径就是普通绝对路径。文本自动分块索引；二进制文件（PDF/Word/图片/jar）以 blob 原样存储并带真实 MIME 类型。PDF 与 Word 额外抽取文本并 embedding——原件保持 byte-for-byte 可下载，内容变得可搜索。
- **Hybrid 搜索** — 每个文本文件自动 chunking、embedding、BM25 索引。三种模式：`hybrid`（dense + BM25，RRF 融合，默认）、`semantic`、`fulltext`。中文分词用 jieba。
- **三层摘要** — LLM 为每个文件生成 L0 摘要（约 100 token）和 L1 概览（约 2k token），目录自底向上聚合。搜索可通过 `detail_level` 返回任意一层。
- **结构化 collection** — schema 先行的表，指定一个自动 embedding 字段；插入 JSON 行、语义搜索、SQL 过滤。
- **向量 workspace** — 裸向量数据面（`kind=db`），适合自带记录的应用：upsert/search/query/delete + 元数据过滤，`write_mode=insert` 提供约 3 倍的批量写入吞吐。
- **SQL** — 内嵌 DataFusion 引擎查询文件和 collection（`SELECT`、`WHERE`、`JOIN`、聚合），另有文件系统操作和向量搜索的 UDF 可在 SQL 里直接用。
- **FUSE 挂载** — `veda-fuse mount` 把 workspace 暴露为本地目录：原生工具开箱即用，write-back 模式吸收编辑器噪音（vim swap 文件、git 锁文件），SSE 保证缓存与远端变更一致。
- **MCP 端点** — server 原生说 Model Context Protocol（`POST /mcp`，Streamable HTTP，stateless）：Coding Agent 一段 `.mcp.json` 即接入，得到 6 个只读工具——search / grep / read_file / list_dir / overview / `ask`（一站式带引用 RAG 问答）。
- **多租户** — Account → Workspace 两级。账号 key（`vk_`）驱动控制面；workspace key（`wk_`，可吊销、有只读变体）驱动数据面。纯 key 校验，无 JWT。

## 工作原理

```
    CLI (veda)     FUSE mount     REST / SSE      Platform gateway
        │              │              │                  │
        └──────────────┴──────┬───────┴──────────────────┘
                              │
                      veda-server (Axum)
              auth: vk_ (control) / wk_ (data plane)
                    ┌─────────┴─────────┐
                    │                   │
                  MySQL              Milvus
             (control plane)      (data plane)
             ┌──────────────┐   ┌────────────────────────┐
             │ accounts     │   │ veda_chunks            │
             │ workspaces   │   │   dense + BM25 sparse  │
             │ dentries     │   │ veda_summaries         │
             │ files        │   │   L0 abstracts         │
             │ file_blobs   │   │ veda_coll_{id}         │
             │ summaries    │   │   structured rows      │
             │ outbox       │   │ ws_<hash>_default      │
             │ datasets     │   │   raw vectors (db ws)  │
             │ schemas      │   └────────────────────────┘
             └──────────────┘
                    │
             Worker (tokio task, outbox consumer)
             chunk → embed → summarize (LLM) → index
```

| 组件 | 技术 | 职责 |
|------|------|------|
| HTTP 层 | Rust, Axum | REST API、SSE 事件流、鉴权中间件 |
| 控制面 | MySQL 8 | 账号、key、路径树、文件内容、outbox 任务队列（ACID） |
| 数据面 | Milvus 2.5+ | Dense ANN + BM25 稀疏向量，RRF hybrid 搜索 |
| SQL 引擎 | DataFusion (Arrow) | 内嵌运行，查询文件 + collection，无额外服务 |
| Embedding / LLM | 任意 OpenAI 兼容 API | Chunk embedding、L0/L1 摘要（LLM 可选） |
| FUSE 客户端 | fuser | 本地挂载，带读缓存 + write-back 缓冲 |
| 可观测性 | Prometheus + OTLP 桥接 | `/v1/metrics` exporter，可选 OTLP gRPC 推送 |

### 关键设计决策

- **Outbox 模式保一致性**：文件写入和它的同步任务（ChunkSync / SummarySync / ExtractSync）在一个 MySQL 事务里提交；后台 worker 把它们回放进 Milvus。默认最终一致，灾难场景用按需漂移修复（`POST /admin/v1/reconcile/{ws}`）。Lease fencing 让多台 server 共享一个 MySQL 时 outbox 依然安全。
- **按内容分层存储**：UTF-8 文本 ≤256KB inline、>256KB 分块；非 UTF-8 存为 blob，MIME 从 magic bytes 判定。内容寻址去重（SHA256），内容没变就跳过写入。
- **承认关键词搜索重要的搜索设计**：dense 向量管语义，BM25 管标识符和精确词，RRF 融合两者——按查询选模式，而不是假装一种 ranker 包打天下。
- **分层上下文加载**：L0/L1 摘要是一等存储对象（存在 Milvus 里可搜索，不是读时计算），专为需要先筛后读的 agent 设计。
- **两种 workspace kind，一套鉴权模型**：`fs` workspace 承载文件 + collection + 摘要；`db` workspace 只承载裸向量。数据面请求都用单次查询的 `wk_` key 校验。
- **简化偏好**：能 MySQL 不上 Kafka，单二进制不上微服务，纯 key 不上 JWT。

## 快速开始

### 前置条件

- Rust 工具链（构建服务端）
- MySQL 8.0+
- Milvus 2.5+（hybrid 搜索依赖 BM25 Function，2.4.x 没有；自带 compose 锁定 v2.5.5，2.6.14 也验证过）
- 一个 OpenAI 兼容的 embedding API
- 可选：一个 OpenAI 兼容的 chat API 用于 L0/L1 摘要（不配则该功能自动禁用）

### 1. 启动依赖

```bash
# MySQL + Milvus（etcd/minio 由 depends_on 自动带起）
cd deploy && cp .env.example .env   # 设置密码 + embedding key
docker compose up -d mysql milvus
cd ..
```

想全部跑在容器里？`docker compose up -d` 构建并运行完整单机栈（依赖 + veda-server + Prometheus）——然后跳过第 2、3 步。

### 2. 配置

服务端默认读 `config/server.toml`，也可以把配置路径作为唯一位置参数传入（`veda-server /etc/veda/config.toml`）。

```bash
cp config/test.toml.example config/server.toml   # 然后编辑
```

```toml
listen = "0.0.0.0:3000"   # 可选——这就是默认值

[mysql]
database_url = "mysql://root:password@localhost:3306/veda"

[milvus]
url = "http://localhost:19530"

[embedding]
api_url = "https://api.openai.com/v1/embeddings"
api_key = "sk-your-key"
model = "text-embedding-3-small"
dimension = 1024

# 可选——启用 L0/L1 摘要
# [llm]
# api_url = "https://api.openai.com/v1/chat/completions"
# api_key = "sk-your-key"
# model = "gpt-4o-mini"
```

所有值可用 `VEDA_*` 环境变量覆盖。MySQL schema 首次启动自动建表（`CREATE TABLE IF NOT EXISTS`，无迁移步骤）。

### 3. 构建并运行

```bash
cargo build --release
./target/release/veda-server
```

### 4. 安装 CLI

从源码构建（`cargo build --release -p veda-cli`），或从任何运行中的服务端拉预编译二进制——每个 server 都自带安装器：

```bash
curl -fL http://<your-server>/install.sh | sh        # 加 --with-fuse 安装 FUSE 客户端
```

### 5. 创建账号和 workspace

```bash
# 零输入匿名开通：server 一次性生成账号 + 默认 workspace + 两把 key
veda init

# 或者同一条命令注册具名账号
veda init --email joe@example.com --password 'something-strong'
```

需要额外 workspace？`veda workspace add <alias>`。从另一台机器导入 key？`veda init --import-key vk_…`（自动备份旧配置）。

## 使用

### 文件操作

```bash
# 上传（本地 → 远程；"-" 读 stdin）。文本二进制都行：
# server 按 UTF-8 sniff——文本走分块 + 索引，二进制存 blob，
# PDF 额外抽取文本层并索引。
veda cp ./README.md /docs/readme.md
veda cp ./design.pdf /docs/design.pdf
veda cp -r ./src /code                   # 递归上传目录

# 浏览
veda ls /docs
veda cat /docs/readme.md
veda cat /docs/readme.md --range 10:20   # 行切片；还有 --head N / --tail N
veda cat /docs/design.pdf > local.pdf    # 二进制 byte-for-byte 无损往返

# 整理
veda mv /docs/old.md /archive/old.md
veda rm /tmp                             # 删除文件或目录（目录递归删除）
```

### 搜索、Grep、摘要

```bash
# Hybrid 搜索（语义 + BM25，默认）
veda search "认证是怎么实现的"

# 单模式
veda search "error handling patterns" --mode semantic
veda search "TODO fix" --mode fulltext

# 限定子树 / 控制返回粒度
veda search "auth" --path /docs
veda search "outbox" --detail-level abstract    # 返回 L0 摘要而非原文 chunk

# 字面量子串扫描（同步，无 embedding 延迟）
veda grep "TODO" /src

# 三层摘要（LLM 生成，异步）
veda abstract /docs/design.pdf    # L0——一句话
veda overview /docs               # L1——结构化概览，目录自底向上聚合
```

### 结构化 Collection

```bash
# 创建——schema 是 JSON 数组；--embed-source 指定自动 embedding 的字段
veda collection create articles \
  --schema '[{"name":"title","type":"string","index":true},
             {"name":"content","type":"string"},
             {"name":"category","type":"string","index":true}]' \
  --embed-source content

# 插入——JSON 数组形式的多行（自动 embedding content 字段）
veda collection insert articles \
  '[{"title":"Intro to Rust","content":"Rust is a systems...","category":"tech"}]'

# 对 embedding 字段做语义搜索
veda collection search articles "systems programming" --limit 10

# 过滤 / 聚合走 SQL
veda sql "SELECT title, category FROM articles WHERE category = 'tech' LIMIT 5"
```

### FUSE 挂载

```bash
veda-fuse mount ~/veda-mount             # 使用 CLI 配置的当前 workspace
vim ~/veda-mount/docs/notes.md           # 原生工具开箱即用
cat ~/veda-mount/docs/.abstract          # 每个目录的只读摘要 sidecar
veda-fuse umount ~/veda-mount
```

默认 daemon 模式，带读缓存和 SSE 驱动的缓存失效。`--write-mode=writeback` 在本地缓冲写入（5 秒防抖），编辑器临时文件不会打到 server。

### 向量 Workspace（Pinecone 风格）

Veda 在 `kind=db` workspace 上提供裸向量数据面——为不需要文件抽象、只要向量存储的应用设计。Schema、默认值和契约见 [`docs/api/vectors.md`](docs/api/vectors.md)。

```bash
# 控制面——账号 key（vk_，由平台/控制台持有）：创建 db kind 的
# workspace，再给它签发 workspace key（wk_）。业务应用只拿 wk_；
# vk_ 留在平台侧。
curl -sS -X POST http://localhost:3000/v1/workspaces \
  -H "Authorization: Bearer $VK" \
  -H "Content-Type: application/json" \
  -d '{"name":"my-vectors","kind":"db","app_id":"my-app"}'

# 数据面——workspace key（wk_）。目标 workspace 绑定在 key 上，
# 请求体不带 workspace_id。text 必填，其余有默认值。
curl -sS -X POST http://localhost:3000/v1/vectors/upsert \
  -H "Authorization: Bearer $WK" \
  -H "Content-Type: application/json" \
  -d '{"records":[
        {"id":"sku-1","text":"Air Jordan 1","meta":{"price":1299}},
        {"id":"sku-2","text":"Yeezy 350","meta":{"price":1599}}]}'

# 搜索——mode 默认 hybrid；semantic/fulltext 需显式指定。
# 请求体同样不带 workspace_id。
curl -sS -X POST http://localhost:3000/v1/vectors/search \
  -H "Authorization: Bearer $WK" \
  -H "Content-Type: application/json" \
  -d '{"query":"sneakers under 1500","mode":"semantic","top_k":5,
       "filter":{"must":[{"field":"meta.price","op":"lt","value":1500}]}}'
```

Java SDK：[`sdk/java`](sdk/java)（`upsert`/`search`/`query`/`delete`、类型化异常、幂等感知重试）。Python 示例：[`examples/python_pinecone_demo.py`](examples/python_pinecone_demo.py)。

## 项目结构

```
veda/
├── crates/
│   ├── veda-types/      # 领域类型、错误定义（零依赖）
│   ├── veda-core/       # trait + 业务逻辑（不含存储实现）
│   ├── veda-store/      # MySQL + Milvus 实现
│   ├── veda-pipeline/   # embedding、chunking、PDF 文本提取、LLM 摘要
│   ├── veda-sql/        # DataFusion SQL 引擎
│   ├── veda-server/     # Axum HTTP 服务端（薄壳）+ outbox worker
│   ├── veda-cli/        # CLI 客户端（二进制名：veda）
│   └── veda-fuse/       # FUSE 挂载（workspace member；--with-fuse 安装）
├── sdk/java/            # db workspace 数据面的 Java SDK
├── web/                 # 落地页 + 用户文档站 + admin 控制台
├── deploy/              # Dockerfile、docker-compose、systemd 单元
├── docs/
│   ├── api/             # API 契约（db-workspace、vectors）
│   ├── plans/           # 活跃计划（索引：docs/design/plans.md）
│   ├── testing/         # 测试 SOP
│   └── archive/         # 已完成 / 已被取代的文档
├── ARCHITECTURE.md      # 系统现状
└── AGENTS.md            # Agent 工作协议
```

## 搜索模式

`POST /v1/vectors/search`（db workspace）和 `veda search`（fs）都接受 `mode`：

| 模式 | 原理 | `score_type` | 适合 |
|------|------|------------|------|
| **hybrid**（默认） | 向量 + BM25，RRF 融合 | `rrf` | 通用 |
| **semantic** | 余弦相似度 | `cosine` | 概念性搜索 |
| **fulltext** | BM25 关键词 | `bm25` | 精确词、标识符 |

不同 `score_type` 的分数不可比。`min_score`（相关性下限）只对 `semantic`/`fulltext` 有效；和 `hybrid` 一起传返回 400（RRF 的 rank 不是相关性分数）。完整契约见 [`docs/api/db-workspace-api.md`](docs/api/db-workspace-api.md)。

## 状态

Alpha。次版本号可能破坏兼容性——见 [`CHANGELOG.md`](CHANGELOG.md)。
尚未实现：图片 OCR、K8s Helm chart。

## 许可证

MIT
