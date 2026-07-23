# Architecture

> 系统现状。实现了什么，什么还没做。每次改架构后更新此文件。

---

## 现状：Phase 1-9 已完成

基础层、存储层、Pipeline、HTTP 层、SQL 引擎、CLI、FUSE、三层信息模型全部实现，含单元测试和集成测试。Phase 8 完成 alpha 稳定化；Phase 9 承接公司向量服务（db kind / `wk_` 数据面 / 平台网关面 / OTLP / Java SDK），生产节点已部署。Phase 索引见 `docs/design/plans.md`。

---

## 模块结构

```
veda-types      领域类型、错误定义、API DTO         (已实现)
veda-core       trait 定义 + 业务逻辑               (已实现)
veda-store      MySQL + Milvus trait 实现           (已实现)
veda-pipeline   embedding、chunking、提取、LLM 摘要   (已实现)
veda-sql        DataFusion SQL 引擎                 (已实现)
veda-server     Axum HTTP 层                        (已实现)
veda-cli        CLI 客户端                          (已实现)
veda-fuse       FUSE 挂载                           (已实现)
veda-tunnel     外部 IM 接入（企微长连接）             (已上生产 .95)
```

## 已实现能力

- `veda-types`：VedaError enum（已移除 `From<String>` 隐式转换，强制显式构造）、ApiResponse、所有领域类型（Account/Workspace/Dentry/FileRecord/FileChunk/OutboxEvent/CollectionSchema/SearchHit/FileSummary/SummaryStatus/DetailLevel 等）、API DTOs；`SearchHit.chunk_index` 为 `Option<i32>`（summary 命中为 None）；12 个单元测试
- `veda-core`：MetadataStore/MetadataTx/VectorStore/TaskQueue/EmbeddingService/LlmService/AuthStore/CollectionMetaStore/CollectionVectorStore trait 定义（含 ping() 健康检查）；FsService（write/read/list/stat/delete/copy/rename/mkdir/append/glob_files/list_dir_recursive，含 dedup/COW/分层存储/outbox，非 UTF-8 内容返回错误而非 panic）；SearchService（hybrid/semantic/fulltext 三模式 + Abstract/Overview/Full 三层分级搜索）；CollectionService（create/list/get/delete/insert_rows/search）；path normalization；SHA256 checksum；glob matching；24 个 mock 单元测试 + 17 个工具单元测试
- `veda-store`：MysqlStore（MetadataStore + MetadataTx + TaskQueue + AuthStore + CollectionMetaStore 实现，启动时 schema bootstrap (`CREATE TABLE IF NOT EXISTS`)，含 veda_summaries 表；`insert_dentry` 使用 MySQL errno 1062 精确匹配 duplicate entry；`claim()` 重入不重复递增 retry_count）；MilvusStore（VectorStore + CollectionVectorStore 实现，REST v2 client，新增 veda_summaries 集合，`post()` 含 3 次指数退避重试）；10 个 MySQL 集成测试 + 2 个 Milvus 集成测试
- `veda-pipeline`：EmbeddingProvider（OpenAI 兼容 HTTP，100/batch 自动分批 + exponential backoff 重试，基于 HTTP status code 判断 retryable）；LlmProvider（OpenAI-compatible chat completions，含 3 次指数退避重试，429/5xx/连接错误自动重试）；semantic_chunk（heading-based + sliding window）；storage_chunk（256KB 边界 + start_line）；extract_text（text/plain 占位）；summary 模块（generate_l0/generate_l1/aggregate_dir_summary + prompt 模板）；6 个 chunking 单元测试 + 3 个 summary 单元测试 + 4 个 embedding 单元测试 + 4 个 embedding 集成测试
- `veda-sql`：VedaSqlEngine（DataFusion session 管理）；FilesTable（递归 dentry 枚举）；CollectionTable（Milvus 查询 → Arrow）；8 个 FS SQL 标量函数（veda_read/write/append/exists/size/mtime/remove/mkdir，友好错误消息）；`embedding()` UDF（文本 → JSON 向量）；`veda_fs()` Table Function（目录列举 / 文件读取 / glob 匹配，CSV/TSV/JSONL/plain text 自动解析）；`search()` UDTF（向量搜索，支持 hybrid/semantic/fulltext 模式 + limit 参数）；`veda_fs_events()` Table Function（事件查询，支持 since_id/path_prefix/limit）；`veda_storage_stats()` Table Function（文件/目录/字节统计）；支持 SELECT/WHERE/COUNT/JOIN 等标准 SQL；37 个 mock 单元测试
- `veda-server`：ServerConfig（TOML 加载 + `VEDA_` 环境变量覆盖，新增 LlmConfig/allowed_origins）；API key（`vk_`）+ workspace key（`wk_`）认证中间件（workspace-key 路径自动填充 account_id；JWT 曾有、2026-06 整体移除）；Account/Workspace/File/Search/Collection/SQL 全部 REST 路由；Worker（outbox 消费 + chunk sync + extract sync + summary sync + dir summary sync）；`GET /v1/summary/{path}` 摘要查询端点；搜索 API 支持 `detail_level` 参数（limit 上限 100）；`/v1/ready` 检查 MySQL+Milvus 可用性（返回 200/503）；SQL 路由使用 Arrow ArrayWriter 直接序列化 JSON；CORS 可配置白名单（`allowed_origins`，默认 permissive）；graceful shutdown；4 个 HTTP 端到端集成测试
- `veda-server`：新增 `POST /v1/fs/{path}` append 路由
- `veda-cli`：clap 命令行解析；account create/login；workspace create/list/use；cp/cat/ls/mv/rm/mkdir/append；search（支持 --detail-level abstract/overview/full）；**ask（`/v1/answer` 非流式,出处按文件去重,--json 供脚本;501/429 友好文案,单请求 100s 超时）**；summary（查看文件/目录摘要）；collection CRUD；sql；config 管理；$HOME/.config/veda/config.toml 持久化；**`$VEDA_SERVER`/`$VEDA_KEY` 环境变量鉴权（与 FUSE 同名,优先级 flag>env>config,零落盘,`veda status` 标注来源）**；**`status --index [--wait]` 索引进度（轮询 `/v1/index-status` 到清零,dead>0 退出码非零;`cp -r` 结束打 queued 提示）**；HTTP client 带 connect 10s / request 60s 超时
- `veda-server`：`GET /v1/index-status`（AuthWorkspace）——workspace 级待索引任务计数 `{pending,processing,dead}`,只统计 chunk_sync/extract_sync（summary 有 30s 防抖,计入会长期非零误导）,SQL 走 `idx_dedup(workspace_id,event_type,status)` 索引
- `veda-tunnel`：企微出处列表按**文件聚合**——citation 是 chunk 粒度（同文件两段=两条同 path citation,server 数据正确）,渲染层同 path 合并为一行携带全部 `[n]`（2026-07-22 生产反馈修复）
- `veda-fuse`：FUSE 文件系统挂载（fuser 0.14），需要 macFUSE（macOS）或 libfuse（Linux）
  - 基础：子命令 mount/umount；daemon 化（fork+setsid，pipe 通知 parent）；mount 选项（--cache-size/--attr-ttl/--allow-other/--read-only/--debug/--write-mode/--write-debounce-ms）
  - HTTP：blocking client（stat/read/write/list/delete/mkdir/rename/read_range），30s connect+request timeout，ClientError 区分 Network/Server/Parse
  - Inode：inode ↔ path 双向映射 + 可配置 attr TTL + nlookup 计数 + forget 回收；`register_local()` 支持 writeback shadow 的高位 ino（`1<<63+counter`）
  - 写入：WriteHandle dirty 标记消除 flush+release 双写；O_TRUNC 正确标记 dirty；setattr 截断走 If-Match CAS；fsync 实现；多 fh 同 ino 写入告警；WriteHandle 持 path 字段，open-unlink-close 不报 ENOENT
  - 缓存：LRU ReadCache（小文件全量缓存，大文件 Range read），generation 计数器防止 invalidate→put 竞态，TTL 与 attr_ttl 统一；dir_cache 使用 Arc 零拷贝共享
  - SSE：后台线程连接 /v1/events，远程变更时失效 attr+read+dir cache；cursor 原子落盘持久化（debounce 1s）
  - Writeback 模式（`--write-mode=writeback`）：`ShadowStore` 缓冲 FUSE 写入到内存（per-file 10MB / total 50MB cap，超 per-file 自动降级到 sync flush，超 total 返 ENOSPC）；`CommitQueue` 单线程 worker（std::thread + mpsc + min-heap deadline，token-based 合并同 path 的 Touch），默认 5s 静默期；create() defer 不打 server，蛋黄到第一次真正的 Touch 才上传；lookup/getattr/readdir/readdirplus 在 writeback 下走 shadow overlay（tombstone 隐藏，pending_children 追加，dedup by basename）；unlink 把 LocalOnly 直接干掉不打 server，Dirty/Clean 才下 DELETE；rename 先打 server 后改 shadow，old_path 自动 tombstone 防止 in-flight PUT 漏到 server；destroy() unmount 时 drain 所有 pending commit
  - 其他：statfs 返回合理值
- `veda-server`：新增 `GET /v1/events` SSE 端点（轮询 veda_fs_events 表，cursor-based）；`GET /v1/fs/{path}` 支持 `Range` header 返回 206 Partial Content
- `web`：Vite 文档站 + 内置 Console；fs workspace 的 `#/console/fs/{workspace_id}` 支持目录浏览、单文件原始上传和下载。数据面 `wk_` 仅存当前浏览器标签页，直接调用原生 `/v1/fs/*`，文本与二进制均可用。
- v0.1.5 批：
  - 搜索：真 BM25 hybrid（dense + sparse RRF via Milvus 2.5 `hybrid_search`），fulltext 改为 BM25 sparse，jieba 分词中文，自动 schema 迁移（drop+rebuild + 全量 ChunkSync 入队）；`SearchHit.score_type` 标 `rrf` / `bm25` / `cosine`；`/v1/search` 路由响应剥离内部字段（vector / workspace_id）
  - Worker：paginated chunk read（chunk_sync + summary_sync 共享 `load_full_content`），`catch_unwind` 隔离 task panic；summary debounce 30s + burst window 5min（`veda_summary_enqueue_total{burst=...}` 计数）；L1 prompt 结构化 + 输出语言策略仅 zh-CN / en（中文或中文+英语→中文，其余→英语）
  - 端点：`/healthz` 轻量存活探针；`POST /v1/grep` 字面量子串扫描；summary 拆成两条路径——`GET /v1/abstract/{path}` 返 L0 abstract（默认便宜路径），`GET /v1/overview/{path}` 才返 L1 overview；两条都是三态响应（200 ready / 202 pending+Retry-After / 501 disabled+Cache-Control:no-store）
  - 配置：`embedding.batch_size`（含 `VEDA_EMBEDDING_BATCH_SIZE`），`last_embedded_content_hash` 水印 + `force_reembed` 标志
  - CLI：`veda --version`，`veda cp -r` 递归目录上传（跳 symlink），`veda grep`，`veda abstract` (L0) + `veda overview` (L1) 两个独立子命令
  - CLI 初始化：单子命令 `veda init` 五模式互斥分发（anonymous / named `--email` / `--login` / `--upgrade` / `--import-key`），`veda status`（配置健康度 + server reachability ping）。`--import-key` 接 `vk_*` 或 `wk_*`，覆写前自动把旧 `config.toml` 备份成 `config.toml.bak.<unix-ts>`
- v0.1.14–0.1.16 批：
  - **二进制 blob + PDF/Word 提取**：`PUT /v1/fs/{path}` 按 body sniff——合法 UTF-8 走原文本路径不变；非 UTF-8 原样存进新表 `veda_file_blobs`（LONGBLOB，`storage_type=blob`），MIME 从 magic bytes 判定（`infer`）。PDF（`source_type=pdf`）与 Word（`source_type=word`，.docx/.doc，含 infer 区分不出子类型的 `application/x-ole-storage`——提取器 FIB magic 兜底拒非 Word OLE）入队 `ExtractSync`：worker 抽文本层（PDF 用 `pdf-extract`；docx 手写 zip+quick-xml 收 `<w:t>`；.doc 手写宽松 CFB reader + Word97 piece table，`veda-pipeline/src/word.rs`，加密/Word95 明确拒），**全文 upsert 进 `veda_file_extracts`（file_id PK + source_sha256 防 stale）再 embed 进 Milvus**——原件 byte-for-byte 可下载，内容可搜索。图片/jar 等其他二进制只存不索引，扫描版 PDF（无文本层）提取为空则只存不索引。`read_file`/预览对可提取 blob 返回**提取全文**（source_sha256 与 blob 当前 checksum 一致才 serve，否则报「提取中」）——`/v1/answer` 的 `read_file` 工具因此可读 PDF/Word 全文。一致性：extracts 行与 blob 同事务删除（覆盖写/删除/orphan 清理三路径），worker 提取失败清行+watermark，读侧 sha 校验兜底；watermark 命中但 extracts 缺失/stale 时 worker 只补提取不重 embed（backfill 自愈，`scripts/backfill-word-extracts.sql`）。`GET` 回真实 `Content-Type`，blob 支持 byte-range、拒绝行读。预览路径（平台数据面/admin）对不可提取二进制返回 `is_binary=true` + 本地化「暂不支持预览」提示而非乱码；`list_dir` 返回真实 `mime_type`/`size_bytes`。覆盖写 index→noindex（text/pdf/word→image 等）会先清旧向量防 orphan
  - **CLI 二进制支持**：`veda cp` 文本二进制都传原始字节（客户端"looks binary"拒绝已删），`veda cat` 整读回原始字节（重定向即无损 round-trip），`--head/--tail/--range` 对二进制明确报错。二进制 cp/cat 需要 server ≥0.1.15（旧 server 对二进制 cp 返 400）
  - **Linux CLI 改 musl 静态产物**：`x86_64-unknown-linux-musl`，任意 glibc 可跑；`veda-fuse` 仍 gnu（动态链 libfuse3）
  - **server 自带安装器**：`GET /install.sh` 返回构建时嵌入的安装脚本；`GET /capabilities` 无鉴权能力探针（FUSE 用它决定是否暴露 summary sidecar）

## MCP 端点 `POST /mcp`（2026-07-22）

Coding Agent（Claude Code/Cursor/Codex）原生接入面：**Streamable HTTP transport 的 stateless 模式**，手写 JSON-RPC（无 SDK 依赖），协议版本只宣称 `2025-06-18`（03-26 要求 batch，stateless 单消息服务器不实现故不宣称）。用户侧零安装——`.mcp.json` 配 `url` + `Authorization: Bearer wk_` 即接入。设计：`docs/archive/plans/coding-agent-kb-plan.md` §4。

- **`veda-server/routes/mcp.rs`**：`POST /mcp`（`AuthWorkspace`，fs only，与 REST 同一道鉴权闸；GET/DELETE 由 axum 自动 405 = 无下行 SSE/无会话）。挂在 30s TimeoutLayer **之外**（`ask` 需 90s），每工具自带超时（普通 30s / ask 95s）。严格 JSON-RPC 2.0 校验（`jsonrpc`/id 类型/params object，非法一律 -32600+id:null；无 id=notification→202）；`MCP-Protocol-Version` header 校验（有且不支持→400，无→放行）。错误分层：协议错→JSON-RPC error，领域错→`isError:true`+可读文本（LLM 可自愈）。**不做 Origin 校验**（rebinding 页面带不上 Bearer，见模块注释）。
- **6 个只读工具**（进程内直调 service 层，非 HTTP 回环）：`search`（hybrid 固定+detail_level 三层，描述里引导「先 abstract 后 read_file」的 token 经济学）/ `grep`（字面量+行号，**每行截 500B**）/ `read_file`（PDF/Word 返提取文本，整读 64KB 截断+行分页，行读同样过 byte cap）/ `list_dir`（flat 截 10k+truncated；recursive 复用服务层 QuotaExceeded 语义，成功即完整）/ `overview`（L1，pending/disabled 双话术）/ `ask`（非流式 `/v1/answer` 语义，**与 REST 共享 per-workspace 并发闸与全部 answer 指标**）。
- **指标**：`veda_mcp_request_seconds{method,outcome}`——method label 只用白名单工具名（防任意 client 字符串撑爆基数）。
- **测试**：9+ 单元（协议校验/版本协商/截断/schema 形状）+ `tests/mcp_http_test.rs` 集成 mega-test（真实 MySQL/Milvus/embedding：协议边角、鉴权、read-only wk_ 全工具、grep 长行截断、worker 驱动的 hybrid 命中、path_prefix 过滤）。
- 已知尾巴：answer 超时后 Engine 任务存活+permit 提前释放（REST/MCP 同款既有行为），见 `docs/todos.md`。

## RAG 问答 `/v1/answer`（2026-07-14 改造为 Agentic）

fs 数据面知识库问答：LLM 经 **OpenAI function calling** 自主多轮调用 `search` / `read_file` 召回,再生成**带可验证引用**的答案。设计见 `docs/plans/veda-answer-agentic.md`(取代 07-10 的 one-shot 组装管线,退役理由在内)。

- **`veda-core/service/answer.rs`**：`AnswerService`(依赖 `ToolExecutor` + `LlmService` 两个 trait;`LiveTools` 为生产实现,包 SearchService+FsService)。流程:用原始 query 预检索(route limit,默认 12)进首条 user 消息 → agentic loop:每轮 `chat_stream`(流式,tools 随带),LLM 回 tool_calls 则执行(search 固定 limit 6 / read_file 截 8000 字符可 offset 续读,path_prefix 硬约束)并以 role=tool 回填,回 content 即终答;≤`answer_max_tool_rounds`(默认 4)轮后强制收尾(不带 tools)。工具错误一律回填文本让 LLM 自愈。**Prompt 分层**:system = 内置知识库协议(工具用法/引用/防注入/拒答,不可覆盖)+ bot persona(请求 `prompt` 字段,空则 `DEFAULT_BOT_PROMPT`)。**Block 注册表**:search hit 按 (path,chunk) 去重编号,read_file 按 path 注册整文件块(citation `spans:[]` = 整文件);`[n]` 对齐生成 citations,零有效引用→citations 留空+`ungrounded`(不回填未引用块:检索恒返回 top-k,回填会把无关路径当出处外泄)。时间预算:loop 总 80s,单次 LLM 尝试 20s,剩余 <25s 不再开工具轮;重试规则=该次调用未向下游转发过 delta。`LlmService` trait:`summarize` + `chat_stream(messages,tools)`(旧 complete* 已删);`veda-pipeline/llm.rs` 含流式 tool_calls 分片拼装器。
- **`veda-server/routes/answer.rs`**：`POST /v1/answer(/stream)`(`AuthWorkspace`,fs only)。挂在 30s TimeoutLayer **之外**、自带 90s 兜底 deadline;per-workspace 并发信号量(`answer_concurrency` 默认 2,超出 429);query ≤1024 字符、`prompt` ≤4000 字符;SSE 事件 `delta`/`reset`(丢弃已积累 delta,罕见的说话后调工具轮)/`tool`(2026-07-15:工具执行前的进度提示 `{name,detail}`,detail=检索词/文件路径 ≤60 字符不含结果,可丢弃)/`final`(权威)/`error`。「没找到」不再是检索空提前返回,而是 LLM 输出固定话术(outcome=empty 按话术判定)。配置:`[llm]` 下 `answer_max_tool_rounds`(4,调 0≈退化 one-shot 应急旋钮)/`answer_max_output_tokens`(1024)/`answer_concurrency`(2);`answer_max_context_tokens` 已删。指标:`veda_answer_request_seconds{outcome}`、`veda_answer_rounds`(loop 失控监控)等;hit_count=累计去重资料块数。
- **veda-tunnel 接入**：`[answer] enabled`(默认 true,改动需重启)优先走 **`/v1/answer/stream`**(SSE:delta 逐段 → tunnel ≥1s 节流刷新企微气泡,final 帧权威含 citations;`tool` 进度事件渲染为「🔍 正在检索:…」/「📄 正在查阅:…」状态行帧填补工具轮静默段,与 interim 共享节流;老 server 404/405 自动回落 `/v1/answer`),回复=答案正文+出处列表;false 回退纯检索直出。错误话术:501→「问答未启用」、429→「太频繁」、502/504→「暂时不可用」。per-bot `prompt`(veda_tunnel_bots 列)随请求透传。
- **验收**（真实 veda_it MySQL+测试 Milvus+airouter）：端到端带引用答案 3.5s；不编造（无关问题固定拒答）；400/501/kind-mismatch 负向全过。P1（SSE 流式、L1 全局题路径）、v1.5（db kind）见 plan §12。

## 平台网关面（AI Workbench / OnePaaS）

公司 AI 平台把 veda 作为存储底座的专用 surface，与原生 `vk_`/`wk_` 面并存：

- **控制面 `apps.rs`**（`/v1/workspace/{workspace}/...`）：`{workspace}` 是平台侧 workspace code（内部存 `app_id`），其下 veda 自己的 workspace 改叫 **project**（按 `id` 定位）。project/dataset/key 生命周期 CRUD + `GET /v1/my/projects`（当前网关用户的项目扁平列表，keyword 过滤 name/description，offset 分页 `page/size/order_by/order`）。**无 veda 凭证**——鉴权外置给平台网关
- **数据面 `project_data.rs`**（`/v1/workspace/{workspace}/project/{id}/...`）：把 `wk_` 数据面（vectors upsert/search/query/delete + fs search/files/file/sql/grep + **fs 上传 `PUT /file?path=`/下载 `GET /file/content?path=`**，上传同 fs.rs 的 UTF-8/blob sniff 分流、下载带 RFC 5987 attachment 头）包装到网关 surface，前端不持 `wk_`。**读写都过外部 authz**（`authz_and_load`，2026-06-23 定）：数据面暴露实际内容，不依赖网关限路径，veda 独立验证用户在该 workspace 的权限。文件预览截断 256KB，二进制返回 `is_binary` 标识
- **`platform.rs`**：网关在 base64 `user` header 里传身份（`GatewayUser`，取 `name`/`displayName` 落 `creator`/`creator_name`），Cookie 透传给平台 authz/workspace-lookup API；直连（无 header）自动回退原生 key 鉴权。首次 `POST` 按 workspace code 自动开户
- **company envelope 中间件**：handler 返 veda `ApiResponse<T>`，中间件改写成公司规范（`Vec<_>` → `{data:[...], page,...}`；单对象 → bare object；create 返 200 非 201）
- **admin surface `admin.rs`**（`/admin/v1/...`，独立 `admin_token` 门控，fail-closed：未配 token 404）：跨租户只读 dashboard（workspaces/files/file 预览/vectors search）+ db 向量写控制台（vectors upsert）；前端在 `web/src/admin.ts`

## veda-tunnel（外部 IM 接入）

独立进程 / crate（`crates/veda-tunnel`，二进制 `veda-tunnel`），把 veda 检索接入外部 IM。veda 数据面的标准 `wk_` 消费者，**veda-server 一行不改**。一期：企业微信智能机器人长连接（WSS `openws.work.weixin.qq.com`）+ 纯检索直出 + 管控面。

- **一 bot 一连接一 key**：每个企微机器人一条长连接 + 一个只读 `wk_`（绑一 workspace）。群 @提问 / 单聊 → 剥 `@` → `POST /v1/search`（Bearer `wk_`）→ 取 top-k `content`+`path` 拼 markdown → 长连接流式回（先 `finish:false` 占位吸收企微 5s 超时，检索完 `finish:true`）。
- **连接生命周期**（`wecom/conn.rs`）：`aibot_subscribe` 订阅 → 30s `ping` 心跳 → 断线/被踢（`aibot_event_callback` 的 `disconnected_event`）指数退避重连；msgid moka TTL 去重防 5s 重推双查；WS sink 由单写循环独占，读循环 / 心跳 / handler 都经 mpsc 投帧。
- **bot 管理（共享 MySQL 表，三入口）**：bot 配置存 MySQL（`veda_tunnel_bots` 表，生产与 veda-server 同库，`store.rs` bootstrap + information_schema 列迁移；`config.toml` 的 `[[wecom.bot]]` 仅首次 seed）。三个写入口收敛到同一张表：① console UI `#/admin/tunnel`（经 nginx `/tunnel/v1/*` → tunnel `:9110/admin/*`，即时生效）② tunnel admin CRUD API（同上）③ **veda-server 平台 API**（`/v1/workspace/{ws}/project/{id}/tunnel/bots`，AI 工作台专用：直写共享表 + 自动 mint/revoke 只读 `wk_`，`routes/tunnel_bots.rs` + `tunnel_bots.rs`，进程间零 RPC）。tunnel 侧 **30s store 轮询 reconcile**（`reconcile::plan` 纯函数 diff：新增 spawn / 变更 respawn / 删除 stop）收敛平台写入，并把 `conn_state` 心跳写回表（仅变化时 UPDATE，`updated_at=updated_at` 保配置时间戳），平台 GET 可见在线状态。
- **问答质量遥测（qa_log）**：每次问答落 `veda_tunnel_qa_log`（query/提问人 user_id/答案原文/outcome 七分类/延迟/引用数/**tool_trace 检索过程**——流式 `tool` 事件按序收集为 JSON 数组 `[{tool,detail}]`（检索词/读过的文件），节流外采集不丢步；两侧 DDL 副本 2026-07-16 起带 tool_trace 列迁移，best-effort 不阻塞回复）；stream 首帧带 `feedback.id` 激活企微点赞点踩，`feedback_event` 回流 `veda_tunnel_qa_feedback`（同人改评价替换）。outcome=no_context 按 server 拒答话术前缀判定（语义检索恒有 top-k，hit 数不可用）——该清单即知识库内容缺口。admin `/admin/stats`+`/admin/qa-log`，console tunnel 页有统计卡+bad case 明细（时间列带提问人，明细行可展开「答案」与「过程」）。**AI 工作台读**（2026-07-15）：veda-server apps surface 增 `GET .../tunnel/qa/stats`（days 1–90 outcome 分布+赞踩）与 `.../tunnel/qa/logs`（分页明细，`outcome`/`down_voted`/`bot_id` 过滤）；qa 两表随 `TunnelBotStore` 幂等 bootstrap（可先于 tunnel 起在新库），**租户隔离**=先查 project 名下 bot_id 集合再 `bot_id IN(...)` 绑定约束，跨 project 的 bot_id 一律 NOT_FOUND，读过外部 authz（明细含用户原文）；`routes/tunnel_bots.rs`+`tunnel_bots.rs`，集成测试 `tests/tunnel_qa_test.rs` 真 MySQL 验隔离。计划：`docs/plans/veda-tunnel-qa-log.md`。
- **管控面**（`admin.rs`，默认 `127.0.0.1:9100`，fail-closed `admin_token`：未配 404 / 错 401）：`GET /admin/bots`（配置+状态，secret 不返回、`veda_key` 脱敏）、`POST/PUT/DELETE /admin/bots[/{id}]`（CRUD，编辑时 secret/key 留空=保留）、`POST /bots/{id}/reconnect`、`POST /admin/reload`（从 MySQL 全量重载）、`GET /healthz`。
- **单实例 + adapter 结构**：企微「新连接踢旧」约束决定全局单实例持 bot 连接（多实例选主划到未来）。`wecom/` 是第一个 adapter，`config`/`registry`/`veda`/`admin`/`store` 通用，未来 `feishu/` 平级新增。依赖复用 workspace，新增 `tokio-tungstenite`（rustls，同 reqwest TLS 栈）+ `sqlx`（MySQL bot store，同 veda-store 客户端）。

**状态**：生产运行中——2026-07-13 迁至专用生产机 **10.79.52.95**（`tdchw-veda-tunnel-1`，systemd `veda-tunnel.service`，连生产 MySQL `veda` 库 + `.85:3000` 内网直连，admin `0.0.0.0:9110` 用生产 token；nginx 两入口 `/tunnel/v1` 反代已切 .95，.161 旧实例 stop+disable）。真机联调（2026-07-09）+ `/v1/answer` RAG 链路（2026-07-10）+ 平台 API/轮询 reconcile（2026-07-13，集成测试全 CRUD+key mint/revoke 不变量真 MySQL 验证）。部署 runbook：`docs/deploy-tunnel.md`；设计：`docs/plans/veda-tunnel-plan.md`；平台契约：APIDoc `docs/veda/tunnel-bot-api.md`。

## Workspace kinds: fs vs db

每个 workspace 在创建时锁定 `kind`，决定它服务哪条 API 通道：

| `kind` | 数据载体 | API 通道 | 用途 |
|---|---|---|---|
| `fs`（默认） | `veda_dentries / files / chunks / summaries` (MySQL) + `veda_chunks / veda_summaries` (Milvus) + `veda_collection_schemas` (structured collections) | `/v1/files/*`, `/v1/search`, `/v1/grep`, `/v1/sql`, `/v1/abstract`, `/v1/overview`, `/v1/collections/*`, FUSE | 文件知识库（既有能力） |
| `db` | `veda_datasets` (MySQL) + per-ws Milvus collection `ws_<hash16>_default` | `/v1/vectors/{upsert,search,query,delete}`, `/v1/workspaces/{ws}/datasets` | Pinecone-style 裸向量服务（公司 app 共享） |

**认证与隔离**（2026-06 起统一 `wk_`）：
- **数据面统一用 workspace key `wk_`**：`AuthWorkspace`（fs：files/search/sql/...）与 `AuthDbWorkspace`（db：vectors）各自校验 `kind`，不匹配返 400 `workspace_kind_mismatch`。`wk_` 绑定单 workspace，所以 vectors 请求体不再带 `workspace_id`；read-only `wk_` 可 search/query，不可 upsert/delete。鉴权是**单次查询**：`veda_workspace_keys` 冗余 `kind`/`account_id`（建 key 时从 workspace 拷贝、之后不可变），`get_workspace_key_by_hash` 只 `JOIN veda_accounts` 验账号 active，**不读 `workspace.status`**——每请求 MySQL 往返从 3 跳（旧 3 表 JOIN + 二次 `get_workspace` + dataset）降到 2 跳（鉴权 1 + dataset 1）。
- **级联停用**：account suspend 靠上面那条鉴权 JOIN 读时即时拦截；workspace archive 走 `delete_workspace`，在**同一事务**里级联 `UPDATE veda_workspace_keys SET status='revoked'`（鉴权不再读 `workspace.status`，靠这步让 key 失效）。
- **控制面用账号 key `vk_`**（`AuthAccount`）：账号 / workspace / dataset / key 生命周期、`/admin/v1/tokens`。`vk_` 不进数据面、不外发；业务方只拿可吊销、分读写的 `wk_`。token 的 `allowed_workspaces` scope 在唯一的 ownership 闸 `load_owned_workspace` 统一强制，所有带 `ws_id` 的控制面路由自动继承——scoped `vk_` 不能越权操作同账号其他 workspace。
- JWT 已移除（无 `POST /v1/workspaces/{id}/token`、无 `jwt_secret`），鉴权全部为纯 key 校验。
- key 生命周期：`POST/GET/DELETE /v1/workspaces/{id}/keys`（list 仅回元数据，明文只在创建时显示一次）。
- **身份自省 `GET /v1/whoami`**（`AuthAnyWorkspace`，收 fs/db 任一 kind 的 `wk_`，不设 kind 闸）：返回 `{workspace_id, kind, permission}`。CLI `veda status` / `veda init --import-key wk_…` 用它把粘贴 key 场景缺失的 workspace id 回填进本地 config（best-effort，旧 server 404 时静默保持 unknown）。

**三类数据集合在 fs workspace 下并存**：dentry/files、structured collections、L0/L1 summaries。**db workspace 只承载** Pinecone-style 裸向量记录，不允许建 file 或 structured collection。

Vector dataset 是 db workspace 内的逻辑分组（内部物理 pk = `{dataset}:{id}`，全 collection 共享 PK 空间；API 只暴露 `id`，pk 不出 wire）。每个 db workspace 创建时自动 bootstrap 一个 `default` dataset，业务方可不指定 dataset 直接 upsert。

**写入语义 `write_mode`**（`POST /v1/vectors/upsert`，2026-06-08）：默认 `upsert` 走 Milvus 幂等 dedup-by-id；`insert` 跳过 dedup+delete，~3x 写吞吐，由调用方保证 id 唯一（重复 id 会产生重复行）。

完整设计：`docs/vectors-merge-plan.md`（鉴权/app_id/write_mode 等章节已被演进推翻，见其头部 2026-06-10 注记）；历史待办已归档：`docs/archive/vectors-merge-backlog.md`（未兑现尾巴并入 `docs/plans/db-workspace-followups.md`）。

### 客户端 SDK

- **Java SDK**（`sdk/java`，独立 Maven 项目，不在 Cargo workspace）：db 数据面 4 端点（upsert/search/query/delete）的 Java 8 封装。Jackson + OkHttp，fluent filter builder，`error_code`→类型化异常（未知码归 `UNKNOWN`），幂等感知重试（id-less upsert 不自动重试），响应前向兼容（`ignoreUnknown`）。单测绿；真实 server 契约测试走 `mvn -P integration verify`（发版 gate，非 CI）。已发布 `csoss.veda:veda-sdk-java:0.0.1-SNAPSHOT` 到内部 ddxq Nexus（2026-06-04）。设计：`docs/archive/plans/java-sdk-db-plan.md`。**⚠ 待适配**：db 数据面 2026-06 已从 `vk_` 改 `wk_`、请求体去掉 `workspace_id`，SDK 的 `apiKey`→`workspaceKey` 改造 + `write_mode` + e2e 重测尚未做（见 docs/todos.md）。
- **Python 示例**：`examples/python_pinecone_demo.py`（无 SDK，裸 HTTP）。

## 可观测性

- **本地 exporter**：`GET /v1/metrics`（Prometheus 文本格式，`metrics_token` 门控；未配 token 时 404 隐藏）。
- **OTLP 桥接**（2026-06-05 上线）：后台任务每 5s 把全量指标转 OTLP gRPC 推公司 Monitor Collector（counter→Sum / gauge→Gauge / histogram→桶差分，attributes+labels 双写；`[otlp]` config + `VEDA_OTLP_*` 灰度开关）。proto vendored 说明见 `crates/veda-server/proto/PROVENANCE.md`。
- **向量数据面三层指标**：`veda_vector_request_seconds`（handler 端到端）/ `veda_vector_store_op_seconds`（store 层）/ `veda_milvus_request_seconds`（Milvus HTTP），label 含 operation/workspace_id/dataset/mode/outcome。
- **outbox 死信可见性**：`veda_outbox_dead_total{event_type}` + `veda_outbox_depth{status}`（30s 采样），告警规则配在公司 Monitor 平台。

## 测试策略

- 单元测试：`cargo test`（全自动，无外部依赖）
- 集成测试：`cargo test -- --ignored`（需要 `config/test.toml` 配置真实 MySQL/Milvus/Embedding 服务；不起 docker，CI 不跑，手动执行）
- 敏感配置：`config/test.toml`（已 gitignore），模板见 `config/test.toml.example`——新环境先 `cp config/test.toml.example config/test.toml`

## 待实现

- Image OCR（PDF/Word 文本提取已实现；扫描版 PDF 与文档内嵌图片仍不索引，原始字节保留在 blob，上 OCR 后 force ExtractSync 全量重刷即可）
- CLI/FUSE 端到端测试、K8s Helm chart
- OTLP trace 二期（协议事实见 `docs/archive/plans/observability-otlp-plan.md` §0）

## 关键设计决策

原始设计提案已归档（`docs/archive/design/design.md`——其中凭证体系 / reconciler / API 面 / 部署形态均已被演进推翻，**勿当现状读**，现状以本文件为准）。摘要：

- MySQL = control plane (元数据、认证、outbox)
- Milvus = data plane (向量搜索、structured collection 数据)
- 文件分层存储：UTF-8 文本 ≤256KB inline，>256KB chunked；非 UTF-8 存 `veda_file_blobs`（LONGBLOB，magic-byte 判 MIME）；可提取 blob（pdf/word）的派生全文存 `veda_file_extracts`（file_id PK + source_sha256，纯缓存可整表重建，与 blob 同事务删除 + 读侧 sha 校验双保险防 stale）
- Content-addressed dedup (SHA256)
- Outbox pattern 实现最终一致性。文件写入与其 ChunkSync/SummarySync 入队在**同一 MySQL 事务**提交，写路径不会漂移。残余漂移来源只有死信任务（`veda_outbox_dead_total` + `veda_outbox_depth{status}` 暴露，告警在 Monitor 平台配）和 Milvus 侧数据丢失（磁盘/运维/破坏式迁移）。**不再有 6h 后台 reconcile loop**；改为按需 `POST /admin/v1/reconcile/{workspace_id}?dry_run=`（ops `metrics_token` 鉴权，默认 dry_run=true 只报告，失败响亮返回 500）
- Account → Workspace 多租户；控制面 `vk_`、数据面 `wk_`，纯 key 校验（JWT 已移除）
- VedaError::Storage 使用 String（而非 anyhow::Error）避免 lib crate 兼容问题
- **三层信息模型 (Tiered Context Loading)**：
  - L0 Abstract (~100 tokens)：文件/目录的一句话摘要，存入 Milvus 做向量搜索
  - L1 Overview (~2k tokens)：结构化概览，按需从 MySQL 批量加载。所有 summary LLM 调用（L0/L1/目录聚合）共用 `max_summary_tokens` 预算（默认 8192）——预算是防截断保险丝而非输出长度控制；推理模型的思考 token 在部分网关后端计入 max_tokens，预算过紧会在思考阶段耗尽产出空 content（2026-07 生产事故）
  - L2 Full (原文 chunk)：现有 chunk 搜索
  - 写入流程：ChunkSync → SummarySync (LLM 并行生成 L0+L1) → DirSummarySync (自底向上聚合，含去重防抖)
  - 文件和目录的 L0 均写入 Milvus `veda_summaries` 集合（Abstract 搜索可命中目录）
  - SummarySync / DirSummarySync 入队前检查去重，避免重复 LLM 调用
  - 搜索 API：`detail_level` 参数控制返回粒度，Abstract/Overview/Full 三级
  - LLM 配置可选，未配置时 summary 功能自动禁用
