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

- `veda-types`：VedaError enum（已移除 `From<String>` 隐式转换，强制显式构造）、ApiResponse、所有领域类型（Account/Workspace/Dentry/FileRecord/FileChunk/OutboxEvent/CollectionSchema/SearchHit/FileSummary/SummaryStatus/DetailLevel 等）、API DTOs；`SearchHit.chunk_index` 为 `Option<i32>`（summary 命中为 None）；单元测试见 `crates/veda-types/`（src 内联 + `tests/types_test.rs`）
- `veda-core`：MetadataStore/MetadataTx/VectorStore/VectorWorkspaceStore/TaskQueue/EmbeddingService/LlmService/AuthStore/CollectionMetaStore/CollectionVectorStore trait 定义（含 ping() 健康检查）；FsService（write/write_blob/read/read_file_raw/read_file_preview/read_file_range/read_file_lines/list/stat/delete/copy/rename/mkdir/append/grep/query_events/glob_files/list_dir_recursive，含 dedup/COW/分层存储/outbox，非 UTF-8 内容返回错误而非 panic）；SearchService（hybrid/semantic/fulltext 三模式 + Abstract/Overview/Full 三层分级搜索；**path_prefix 作用域下推**（2026-08-11）：带前缀时先枚举子树 dentries（上限 `SCOPE_CAP=1000`，超限退回全局检索+后置过滤并 warn），把 file_ids 下推 Milvus `file_id in [...]`（chunk 面）/ file_ids+dir dentry_ids 下推 `id in [...]`（摘要面），检索在目录内部排序而非「全局 top-K 再过滤」——修复小目录在大库中被挤出候选窗的必然空结果（生产 FDC workspace 复现：4% 占比目录泛词 0/30 候选）；prefix 经 `path::normalize_lenient` 宽容归一（补前导斜杠/吃尾斜杠），空子树/不存在路径短路返回空，边界匹配修正（`/api-docs` 不再吞 `/api-docs-v2`）；目录摘要命中经 `get_dentry_paths_by_ids` 二次解析出 path（此前恒 None、带前缀时被静默丢弃），dir 命中不计入热度统计）；AnswerService（agentic RAG 问答，详见下文 `/v1/answer` 章节）；CollectionService（create/list/get/delete/insert_rows/search）；**VectorService**（db-kind 向量数据面全部业务逻辑：validate→dedupe→embed→写入、search/query/delete、`veda_vector_request_seconds` 计时随逻辑走——原生 `wk_` 面与平台网关面共用同一实例，2026-08-05 从 routes/vectors.rs 下沉）；**WorkspaceService**（workspace/租户开通：`create_workspace`（db kind 含 MySQL+Milvus 跨存储 saga 与回滚顺序）、`ensure_account`/`lookup_active_account` 平台租户自动开通——`vk_` 控制面与平台面共用，2026-08-05 从 routes/account.rs+apps.rs 下沉）；**`milvus` 模块**（Milvus 约定单点：`milvus_quote` 表达式转义、`vector_collection_name` 命名、`to_milvus_expr` Filter DSL 编译，veda-store re-export 前两者——转义此前在 admin.rs/filter.rs/milvus.rs 三份拷贝，2026-08-05 收敛）；path normalization；SHA256 checksum；glob matching；mock 单元测试 + 工具单元测试见 `crates/veda-core/`（src 内联 + `tests/fs_service_test.rs` / `tests/search_test.rs`）
- `veda-store`：MysqlStore（MetadataStore + MetadataTx + TaskQueue + AuthStore + CollectionMetaStore 实现，启动时 schema bootstrap (`CREATE TABLE IF NOT EXISTS`)，含 veda_summaries 表；`insert_dentry` 使用 MySQL errno 1062 精确匹配 duplicate entry；`claim()` 重入不重复递增 retry_count；**2026-08-05 起为 `mysql/` 模块目录**——`mod`(struct/pool/共享 helper)/`rows`(row_to_* 映射)/`schema`(migrate 全部 DDL)/`metadata`/`tx`/`queue`/`auth`/`collection` 按 trait 边界一文件一职责，原 3483 行单文件拆散，对外路径 `veda_store::mysql::{MysqlStore,PoolConfig}` 不变）；MilvusStore（VectorStore + CollectionVectorStore 实现，REST v2 client，新增 veda_summaries 集合，`post()` 含 3 次指数退避重试）；真实依赖集成测试见 `crates/veda-store/tests/`（`mysql_test.rs` / `milvus_test.rs`）
- `veda-pipeline`：EmbeddingProvider（OpenAI 兼容 HTTP；**两级优先并发闸** `TwoLevelGate`（`max_concurrency` 默认 8：交互调用（search/ask/同步 vectors 写）永远拿到下一个释放的 permit，空闲时 worker 后台索引（经 `background()` 低优先视图）可占满全部；429 backoff 期间还号、acquire 无超时（调用方 deadline 兜底）、取消的等待者派号时被跳过）；按 batch_size 切块 `buffered(4)` 并发直发；exponential backoff/Retry-After(60s cap) 重试，基于 HTTP status code 判断 retryable；指标 inflight/permit_wait{priority}/429/batch_texts）；LlmProvider（OpenAI-compatible chat completions；错误分三类 Quota(429)/Transient(5xx、连接失败、空 content)/Terminal(4xx、无法解析)，前两类走 3 次重试——quota 用 1s/3s/9s ± 抖动、其余保持 0.5/1/2s；quota 耗尽且配了 `summary_fallback_model` 时换模型再发一次同形请求，指标 `veda_llm_fallback_total`；仅摘要路径，`chat_stream` 不重试不降级）；semantic_chunk（heading-based + sliding window）；storage_chunk（256KB 边界 + start_line）；extract_text（`text/plain` 直返、PDF 抽文本层、`.docx`/`.doc` 抽正文；不支持的 mime 明确报错）；summary 模块（generate_l0/generate_l1/aggregate_dir_summary + prompt 模板）；chunking/summary/embedding 单元测试（含 TwoLevelGate 六个行为测试）+ 真实端点 embedding 集成测试
- `veda-sql`：VedaSqlEngine（DataFusion session 管理）；FilesTable（递归 dentry 枚举）；CollectionTable（Milvus 查询 → Arrow）；8 个 FS SQL 标量函数（veda_read/write/append/exists/size/mtime/remove/mkdir，友好错误消息）；`embedding()` UDF（文本 → JSON 向量）；`veda_fs()` Table Function（目录列举 / 文件读取 / glob 匹配，CSV/TSV/JSONL/plain text 自动解析）；`search()` UDTF（向量搜索，支持 hybrid/semantic/fulltext 模式 + limit 参数）；`veda_fs_events()` Table Function（事件查询，支持 since_id/path_prefix/limit）；`veda_storage_stats()` Table Function（文件/目录/字节统计）；支持 SELECT/WHERE/COUNT/JOIN 等标准 SQL；**规划器硬闸**：`SQLOptions` 关闭 DDL/DML/statements（**无条件生效，与 `read_only` 无关**），堵死 `COPY … TO` / `CREATE EXTERNAL TABLE` 以 server uid 读写宿主机文件的逃逸路径；查询整体带 `SQL_QUERY_TIMEOUT`；mock 单元测试见 `crates/veda-sql/tests/sql_test.rs`
- `veda-server`：ServerConfig（TOML 加载 + `VEDA_` 环境变量覆盖，新增 LlmConfig/allowed_origins）；API key（`vk_`）+ workspace key（`wk_`）认证中间件（workspace-key 路径自动填充 account_id；JWT 曾有、2026-06 整体移除）；Account/Workspace/File/Search/Collection/SQL 全部 REST 路由；Worker（outbox 消费 + chunk sync + extract sync + summary sync + dir summary sync）；`GET /v1/abstract/{path}` / `GET /v1/overview/{path}` 摘要查询端点（三态响应，见下）；搜索 API 支持 `detail_level` 参数（limit 上限 100）；`/v1/ready` 检查 MySQL+Milvus 可用性（返回 200/503）；SQL 路由使用 Arrow ArrayWriter 直接序列化 JSON；**CORS 默认拒绝跨域**（`allowed_origins` 为空且 `dev_mode=false` → `CorsLayer::new()`，同源仍可用）；配 `allowed_origins` 白名单放行；`dev_mode=true` 才退化为 permissive（仅本地开发，启动时 warn）；retention sweep（见下）；graceful shutdown + systemd socket activation；HTTP 端到端集成测试见 `crates/veda-server/tests/`
- `veda-server`：新增 `POST /v1/fs/{path}` append 路由
- `veda-cli`：clap 命令行解析；`init` 五模式（建账/登录已收敛进来，无独立 `account` 子命令）；workspace add/list/switch/rm（别名 `ws`）；cp/cat/ls/mv/rm/mkdir/append；search（支持 --detail-level abstract/overview/full）；**ask（`/v1/answer` 非流式,出处按文件去重,--json 供脚本;501/429 友好文案,单请求 100s 超时）**；`abstract`（L0）/ `overview`（L1）两个独立子命令（无 `summary` 子命令）；collection CRUD；sql；config 管理（`#[command(hide)]` 隐藏，排错用）；$HOME/.config/veda/config.toml 持久化；**`$VEDA_SERVER`/`$VEDA_KEY` 环境变量鉴权（与 FUSE 同名,优先级 flag>env>config,零落盘,`veda status` 标注来源）**；**`status --index [--wait]` 索引进度（轮询 `/v1/index-status` 到清零,dead>0 退出码非零;目录 `cp` 结束打 queued 提示）**；HTTP client 带 connect 10s / request 60s 超时
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
  - 配置：`embedding.batch_size`（含 `VEDA_EMBEDDING_BATCH_SIZE`）、`embedding.max_concurrency`（含 `VEDA_EMBEDDING_MAX_CONCURRENCY`），`last_embedded_content_hash` 水印 + `force_reembed` 标志
  - CLI：`veda --version`，`veda cp` 目录自动递归上传（src 是目录即递归，跳 symlink；**没有 `-r` 参数**），`veda grep`，`veda abstract` (L0) + `veda overview` (L1) 两个独立子命令
  - **`veda cp` 目录上传的 ignore 规则**（`ignore` crate 0.4.31）：只认**源目录树内**的 `.gitignore` 与 `.vedaignore`（同 gitignore 语法），外加内置兜底列表（`.git`/`__pycache__`/`.idea`/`node_modules`/`.DS_Store`）。**刻意不读** `.ignore`（ripgrep 约定且优先级高于 `.gitignore`）、全局 gitignore、`.git/info/exclude`、以及源根**之上**的任何 ignore 文件——它们会让同一目录在不同机器上传出不同内容。四个正确性所系的点：
    - `hidden(false)`：crate 默认跳过所有 dotfile，不关掉会静默漏传 `.github/`、`.env.example`、`.cursor/rules`
    - **`.gitignore` 走 `add_custom_ignore_filename` 而非 `git_ignore(true)`**：`Ignore::add_parents` 只在 `parents`/`git_ignore`/`git_exclude`/`git_global` **四个全 false** 时才短路，留着 `git_ignore(true)` 的话 walker 仍会一路爬到文件系统根去找 ignore 文件。祖先规则确实不参与匹配（`matched_ignore` 在 `is_absolute_parent` 处 `take_while` 停止），但**照样被解析**——某个 `~/.gitignore` 里一个畸形 glob 就会冒泡成 walk error 中止一次无关的上传。改走 custom 后无祖先探测、也不再依赖源目录是不是 git 仓库（`require_git` 随之成为无关项，已删）。注册顺序即优先级：先 `.gitignore` 后 `.vedaignore`，故 `.vedaignore` 可覆盖 `.gitignore`
    - **必须检查 `DirEntry::error()`**：ignore 文件的解析错误不会让 walk 失败，crate 把它挂在**成功**的目录条目上并带着更少的规则继续走。吞掉它 = `.gitignore` 打个错字就静默上传整个 `target/`，正是本功能要防的失败模式，故视为 fatal。（globset 容忍 `[[[bad` 这类不平衡括号，真会报错的是 `[z-a]` 非法区间、`a\` 悬空反斜杠）
    - 兜底列表走 `filter_entry` **下降前剪枝**——放在迭代循环里按名字判断的话，walker 会进入 `.git/` 并吐出 `.git/objects/ab/cd`（名字是 `cd`，匹配不到任何规则）导致整个目录被上传

    `--no-ignore` 关掉 gitignore 规则但**保留**兜底剪枝。实测本仓库：342 个文件 vs `--no-ignore` 的 511,682 个（`target/` 独占 511,261 个）
  - CLI 初始化：单子命令 `veda init` 五模式互斥分发（anonymous / named `--email` / `--login` / `--upgrade` / `--import-key`），`veda status`（配置健康度 + server reachability ping）。`--import-key` 接 `vk_*` 或 `wk_*`，覆写前自动把旧 `config.toml` 备份成 `config.toml.bak.<unix-ts>`
- v0.1.14–0.1.16 批：
  - **二进制 blob + PDF/Word 提取**：`PUT /v1/fs/{path}` 按 body sniff——合法 UTF-8 走原文本路径不变；非 UTF-8 原样存进新表 `veda_file_blobs`（LONGBLOB，`storage_type=blob`），MIME 从 magic bytes 判定（`infer`）。PDF（`source_type=pdf`）与 Word（`source_type=word`，.docx/.doc，含 infer 区分不出子类型的 `application/x-ole-storage`——提取器 FIB magic 兜底拒非 Word OLE）入队 `ExtractSync`：worker 抽文本层（PDF 用 `pdf-extract`；docx 手写 zip+quick-xml 收 `<w:t>`；.doc 手写宽松 CFB reader + Word97 piece table，`veda-pipeline/src/word.rs`，加密/Word95 明确拒），**全文 upsert 进 `veda_file_extracts`（file_id PK + source_sha256 防 stale）再 embed 进 Milvus**——原件 byte-for-byte 可下载，内容可搜索。图片/jar 等其他二进制只存不索引，扫描版 PDF（无文本层）提取为空则只存不索引。`read_file`/预览对可提取 blob 返回**提取全文**（source_sha256 与 blob 当前 checksum 一致才 serve，否则报「提取中」）——`/v1/answer` 的 `read_file` 工具因此可读 PDF/Word 全文。一致性：extracts 行与 blob 同事务删除（覆盖写/删除/orphan 清理三路径），worker 提取失败清行+watermark，读侧 sha 校验兜底；watermark 命中但 extracts 缺失/stale 时 worker 只补提取不重 embed（backfill 自愈，`scripts/backfill-word-extracts.sql`）。`GET` 回真实 `Content-Type`，blob 支持 byte-range、拒绝行读。预览路径（平台数据面/admin）对不可提取二进制返回 `is_binary=true` + 本地化「暂不支持预览」提示而非乱码；`list_dir` 返回真实 `mime_type`/`size_bytes`。覆盖写 index→noindex（text/pdf/word→image 等）会先清旧向量防 orphan。**摘要**：PDF/Word 的文本层只在提取后才存在，所以它们的 `SummarySync` 由 worker 在 `upsert_file_extract` 之后入队（不是写入时——那会儿还没有可摘要的文本），因而与文本文件一样有 L0/L1，含它们的目录聚合也把它们算进去。三处 blob 判定共用同一条新鲜度规则（`source_sha256 == checksum_sha256`，`worker.rs::fresh_extract`）：extract 是否需要重跑、`summary_sync` 是否跳过、`load_full_content` 是否返回提取全文——stale 的 extract 一律当作不存在，否则摘要会描述文件已经不再持有的内容。图片/jar 没有 extract，`summary_sync` 照旧**跳过而非报错**（2026-07-13 那次 315 条 dead-letter 的加固）；`/v1/abstract` 对它们回 `415 UNSUPPORTED_FILE_TYPE` 而不是永远 202。存量（0.1.20–0.1.25 期间上传、从未生成摘要的 PDF/Word）用 `scripts/backfill-blob-summaries.sql` 重刷
  - **CLI 二进制支持**：`veda cp` 文本二进制都传原始字节（客户端"looks binary"拒绝已删），`veda cat` 整读回原始字节（重定向即无损 round-trip），`--head/--tail/--range` 对二进制明确报错。二进制 cp/cat 需要 server ≥0.1.15（旧 server 对二进制 cp 返 400）
  - **Linux CLI 改 musl 静态产物**：`x86_64-unknown-linux-musl`，任意 glibc 可跑；`veda-fuse` 仍 gnu（动态链 libfuse3）
  - **server 自带安装器**：`GET /install.sh` 返回构建时嵌入的安装脚本；`GET /capabilities` 无鉴权能力探针（FUSE 用它决定是否暴露 summary sidecar）

## Workspace 布局 `GET /v1/layout`（2026-08-03）

一次调用给出「这个知识库整体是什么」：顶层条目 + 每条 L0 摘要 + 文件数 + 全局统计。**零新增 LLM 调用**，纯组装既有摘要数据。目标读者是 MCP 接入的 coding agent（陌生 workspace 的第一次调用，替代反复 `list_dir` 摸索）与 tunnel 机器人的「你知道些什么」。

- **它就是根级视图**。workspace 根 `/` 没有 dentry，因而没有自己的 L0/L1 行（`routes/search.rs` 头部注释记着 2026-05-14 评审删掉裸 `/v1/abstract` 根路由的原因）。map 用顶层子节点确定性组装出根级视图，从而**不需要**教 worker 去生成根摘要。
- **`veda-core/service/search.rs::workspace_map`**：6 次 store 往返，**每一步都有界**——先 `list_children_capped(ws, "/", CAP+1)`（SQL 层 `ORDER BY is_dir DESC, path LIMIT ?`，多取 1 行判断截断，无需额外 COUNT），截到 CAP 后才按这些 id 批量取 file metadata 与摘要。反过来做（先读全部再截断）会在根下散着几万文件的 workspace 上重演 review C2 的 OOM。
- **新增 3 个 `MetadataStore` 方法**（纯增量，**没动任何既有签名**，worker/reconciler/SQL 引擎零改动）：`list_children_capped`、`get_summaries_by_dentry_ids`（`get_summaries_by_file_ids` 的目录侧对称版，**不给 trait 默认实现**——默认实现会静默退化成 N+1）、`count_files_by_top_level`。
- **`count_files_by_top_level` 的成本要说实话**：`GROUP BY SUBSTRING_INDEX(SUBSTRING(path,2),'/',1)` 是表达式分组，`veda_dentries` 上**没有索引能服务它**（该表只有 `idx_ws_path`/`idx_parent`/`idx_ws_path_prefix`），只能靠复合索引左前缀限定 workspace 后全量扫描，即 `O(workspace dentry 数)`。与 map 同时调用的 `storage_stats` 本来就是同量级，故未引入新的复杂度量级——但**上线前需在生产量级 `EXPLAIN ANALYZE`**，不可接受时退路是砍掉 `file_count`。根下的文件（`/README.md`）会分组到 key `README.md`，组装时**只给 `is_dir` 的条目读这个 count**，不能按「map 里有没有这个 key」判断。
- **`summary_state` 三态**（`ready`/`partial`/`disabled`）用 body 字段而非 HTTP 三态——map 是 N 条摘要的聚合，套不上 `/v1/abstract` 的 202/501。两处易错语义：`disabled` **不清空已缓存的 abstract**（`/v1/abstract` 在有 summary 时根本不看 `summary_enabled`，藏起来会自相矛盾）；`partial` 只是「覆盖率不完整」的事实陈述，**不承诺重试有用**（变空的目录其摘要会被 worker 主动删除且不再生成）。`summary_enabled` 是 server 层状态，core 只判 ready/partial，由 handler 覆写 `disabled`。
- **规模上限 `MAP_ENTRY_CAP = 200`**（`routes/search.rs`），超出置 `truncated: true`；排序目录在前、文件在后，故截断优先保住信息密度高的目录。`stats` 始终描述整个 workspace，不受截断影响。该值是拍的，无生产数据支撑。
- **暂不接**平台网关面与 tunnel：tunnel 是标准 `wk_` 消费者，要用直接调 `/v1/layout` 即可，server 侧零工作。CLI 侧提供 `veda layout`（人类可读块状布局：头行 + 缩进的完整 L0，TTY 按终端宽度折行、管道不折行；`--json` 供 agent）。
- **测试**：8 条组装单测（mock 可注入摘要/计数，含「cap 必须下推到查询」的 `limit == 201` 断言——只看返回长度无法区分 load-all-then-truncate）+ `tests/map_test.rs` 真实 MySQL/Milvus 集成（SUBSTRING_INDEX 计数、目录优先序、250 条截断、鉴权 401/400、MCP↔REST 同构、disabled 仍返缓存摘要）。摘要用 `upsert_summary` 直接写入而非跑 LLM worker——要验的是 SQL，接 LLM 只会增加不确定性。

## 文档访问热度统计 `GET /v1/stats/docs`（2026-08-05）

fs workspace 按文档按天计数 `search_hits`（搜索曝光，按 query 去重）与 `reads`（内容被服务端实际取出），业务方看「哪些文档在被用」。设计与评审裁决：`docs/archive/plans/doc-access-stats.md`（Claude+Codex 交叉评审后修订）。

- **采集**：`veda-core/service/access_stats.rs::AccessRecorder`——进程内 `Mutex<HashMap>` 聚合（热路径纳秒级，单写者架构约束下安全），server 后台任务 30s 批量 `INSERT…ON DUPLICATE KEY UPDATE` 进 `veda_doc_access_daily`（PK `(workspace_id, day, dentry_id)` + `idx_day`；`read_count` 列名避开 MySQL 保留字 READS）。**flush 全量单事务，失败整体丢弃不重试**（重试无法 exactly-once，双计比丢一个 30s 窗口更糟）；final flush 在 `axum::serve` 返回后由 main 显式执行（在 shutdown 信号时刻 flush 会丢 drain 窗口的尾巴）。表清理由 stats 任务自己每日执行（`[stats] retention_days`，不受 `[retention].enabled` 影响）。
- **聚合 key = `dentry_id`**：覆盖写/rename 下唯一稳定的身份（`file_id` 在 `ref_count>1` 覆盖写会换、copy 别名会合并；`path` 每 rename 断一次）。搜索侧经 `get_dentry_paths_by_file_ids`（已改返回 `DentryPathRef{dentry_id,path}`，`ORDER BY path` 保证 copy 别名归属确定）随 resolve 顺带填进 `SearchHit.dentry_id`（serde skip，server-only），零额外往返；resolve 不到的命中（游离 file_id / dir-summary 命中）无 dentry_id 自动跳过。
- **埋点边界（评审核心裁决）**：只在公开方法最外层记一次——`FsService` 五个 read 方法（`read_file`/`read_file_raw`/`read_file_preview`/`read_file_range`/`read_file_lines`）成功路径 + `SearchService::search` 最终命中集（截断/prefix 过滤后）。内部复用走不计数的 `read_file_inner`/`read_range_resolved`：**grep 逐文件扫描零计数**（否则一次全库 grep 把 5 万文件全刷成已读）、preview 委托 range 不双计、不支持预览的二进制占位响应不算读。**SQL 面整体豁免**：组装时给 SQL engine 一个不带 recorder 的 `FsService` 实例（`FsService::new` 默认 disabled recorder，生产 fs/search 用 `with_stats` 显式接入）。worker 后台读走 store 直读天然不计。
- **查询**：`GET /v1/stats/docs?days&limit&order_by`（`AuthWorkspace` fs-only，read-only `wk_` 可查）+ 平台面 `GET .../project/{id}/stats/docs`（同一 `build_doc_stats`，company envelope 展开）。SQL 按 PK 左前缀 range + join 活 dentry（rename 历史延续、删除自动消失）。天边界按 `[stats] day_utc_offset_hours`（默认 +8）在 Rust 侧换算,不依赖进程 TZ 与 MySQL 会话时区。
- **配置** `[stats]`：`enabled`（kill switch,关闭停记录但查询照常）/ `flush_interval_secs`（≥5s）/ `retention_days` / `day_utc_offset_hours`,全部 `VEDA_STATS_*` 可覆盖。指标：`veda_doc_access_flush_seconds{outcome}` / `flushed_rows_total` / `dropped_total` / `retention_swept_total`。
- **测试**：10 条单测（聚合/跨天分桶/失败丢弃/去重/不可归属跳过/grep 零计数/preview 恰一次/二进制预览不计/默认构造静默）+ `tests/stats_http_test.rs` 真实 MySQL 集成（端到端计数、grep 豁免、rename 延续、删除消失、排序、401/400/kind 闸、read-only 可查）。
- **暂不计**（v1 候选）：来源维度（人/agent）、citation 计数、摘要消费（abstract/overview/layout）、SQL `search()` UDTF（本就绕过 SearchService,收敛是独立还债项,见 todos）。
- **展示面**：admin 后台 workspace 详情页有「文档热度」区块（2026-08-06,见平台网关面 admin surface 节）；业务方侧 v0 仍为纯 API,console/CLI 展示按需后补。

## MCP 端点 `POST /mcp`（2026-07-22）

Coding Agent（Claude Code/Cursor/Codex）原生接入面：**Streamable HTTP transport 的 stateless 模式**，手写 JSON-RPC（无 SDK 依赖），协议版本只宣称 `2025-06-18`（03-26 要求 batch，stateless 单消息服务器不实现故不宣称）。用户侧零安装——`.mcp.json` 配 `url` + `Authorization: Bearer wk_` 即接入。设计：`docs/archive/plans/coding-agent-kb-plan.md` §4。

- **`veda-server/routes/mcp.rs`**：`POST /mcp`（`AuthWorkspace`，fs only，与 REST 同一道鉴权闸；GET/DELETE 由 axum 自动 405 = 无下行 SSE/无会话）。挂在 30s TimeoutLayer **之外**（`ask` 需 90s），每工具自带超时（普通 30s / ask 95s）。严格 JSON-RPC 2.0 校验（`jsonrpc`/id 类型/params object，非法一律 -32600+id:null；无 id=notification→202）；`MCP-Protocol-Version` header 校验（有且不支持→400，无→放行）。错误分层：协议错→JSON-RPC error，领域错→`isError:true`+可读文本（LLM 可自愈）。**不做 Origin 校验**（rebinding 页面带不上 Bearer，见模块注释）。
- **7 个只读工具**（进程内直调 service 层，非 HTTP 回环）：**`layout`**（workspace 顶层布局，无参数，等价 `GET /v1/layout`；**在 `tools_specs()` 数组里排第一且 `initialize` 的 instructions 首句就引导它**——工具存在但 instructions 不提，agent 基本不会调）/ `search`（hybrid 固定+detail_level 三层，描述里引导「先 abstract 后 read_file」的 token 经济学）/ `grep`（字面量+行号，**每行截 500B**）/ `read_file`（PDF/Word 返提取文本，整读 64KB 截断+行分页，行读同样过 byte cap）/ `list_dir`（flat 截 10k+truncated；recursive 复用服务层 QuotaExceeded 语义，成功即完整）/ `overview`（L1，pending/disabled 双话术）/ `ask`（非流式 `/v1/answer` 语义，**与 REST 共享 per-workspace 并发闸与全部 answer 指标**）。
- **指标**：`veda_mcp_request_seconds{method,outcome}`——method label 只用白名单工具名（防任意 client 字符串撑爆基数）。
- **测试**：9+ 单元（协议校验/版本协商/截断/schema 形状）+ `tests/mcp_http_test.rs` 集成 mega-test（真实 MySQL/Milvus/embedding：协议边角、鉴权、read-only wk_ 全工具、grep 长行截断、worker 驱动的 hybrid 命中、path_prefix 过滤）。
- 已知尾巴：answer 超时后 Engine 任务存活+permit 提前释放（REST/MCP 同款既有行为），见 `docs/todos.md`。

## Agent/团队记忆 M1（2026-08-12）

fs workspace 的第三类资产：一句话一条的原子记忆，个人/团队归属分域。设计权威 `docs/design/agent-memory.md`（18 节 + 三批拍板），施工图 `docs/plans/agent-memory-m1.md`。

- **数据模型**：`veda_memories`（BIGINT id 供 `[mem:123]` 引用格式；`(scope_type, scope_id)` 归属分域——`workspace`=团队域 / `principal`=个人域；个人域 `origin_workspace_id` 区分项目笔记与随身偏好；`UNIQUE(scope_type, scope_id, content_hash)` 把精确去重压到 DB 层；**零状态列**——错→UPDATE、不要→硬删、到期→`expires_at` SQL 侧 `NOW()` 过滤）+ `veda_principals`（`(source, external_id)` 唯一，首见 lazy 建，M1 仅 key 源：`AuthWorkspace.key_id` → principal，一 key 一人）。`last_used_at` 检索命中即 touch（`updated_at=updated_at` 钉住审计列），排序权重 M2 再调。
- **Milvus 纯索引**：共享 collection `veda_memories` 只存 id + 三个域标量 + vector，**content/kind/topic 刻意不进**——读路径定死「Milvus 只产候选（over-fetch 2×），MySQL 复核带同一 scope 条件为唯一权威」，因此 Milvus 任何失败只降召回不损正确性：漏写→outbox `MemorySync` 自愈（worker 按 payload 里的 scope 重读→重嵌→补写）、残留→复核查无此行自动消失、跨域混入→复核第二层挡。GateMem 两断言（越权检出=0/删后检出=0，含手工制造 Milvus 残留窗口）是 `tests/memory_http_test.rs` 的确定性断言。
- **`veda-core/service/memory.rs`**：save 七步（embed 交互闸→同域近邻 top-3 返回引导改旧条→topic 缺省继承 top-1 近邻（cos≥0.75）→幂等 INSERT→Milvus 同步写失败入 outbox→重试撞唯一键=Duplicate 顺手补向量自愈）；scope 三档 `mine`（缺省）/`team`/`self`（M1 key 身份下 mine≡self）；origin 按 kind 默认（preference 随身、其余锁当前 workspace，显式 `""` 强制随身，永不报错）；update/delete 的 WHERE 带调用方可写域集合，跨域探测得 404 不泄露存在性。
- **接入面**：REST `POST/PATCH/DELETE /v1/memory{,/id}` + `GET /v1/memory/search|context`（`AuthWorkspace` fs-only，read-only `wk_` 可读不可写）；MCP 工具 7→12（`memory_context/save/update/delete/search`，与 REST 共用 api.rs DTO + `deny_unknown_fields`），**首批写型工具**——specs 断言从「全 readOnly」改为 writer 白名单；工具 description 承载全部治理引导（scope 判据「知识关于谁」、宁缺毋滥三原则、近邻≥0.85 提示改旧），initialize instructions 引导开工先调 `memory_context`。零新配置键（唯一外部依赖 embedding 本就必配）。
- **测试**：service 6 单测（scope/origin 解析、topic 继承阈值、双写失败入队、Duplicate 自愈、复核剔除+触点）+ store 层 `veda-store/tests/memory_test.rs`（真实 MySQL/Milvus：context 域合并 origin 过滤、过期过滤、touch 审计不变量）+ `veda-server/tests/memory_http_test.rs` mega（REST+MCP 端到端 + GateMem 两断言）。
- **M2a answer 双源（2026-08-13）**：团队域记忆成为 answer 的第二证据源——`answer_stream` 初检后对当前 workspace 做一次 `team_memories` 检索（`MEMORY_INJECT_LIMIT=5`，无分数下限，复用候选→复核→touch 链路，检索即计数），命中按注入模板 `[n] 记忆(kind·日期·署名) <<<content>>>` 与文件初检并列进首条 user 消息；检索失败降级为空（记忆索引只降召回不挡回答）。`AnswerCitation.path` 改 `Option`，记忆引用带 `memory:{id,content,updated_by,updated_at}` 无 path——旧消费者对无 path 条目自动跳过出处行（正文 `[n]` 保留），server 先上无部署顺序约束。tunnel 出处列表渲染 `[n] 记忆：content`（折叠行「等 N 篇」→「等 N 条」）；CLI ask 打 `记忆:` 行；MCP ask 的 citations JSON 原样携带。**个人域不进 answer**（需操作者身份，随 M3 身份透传一起做）。施工图 `docs/plans/agent-memory-m2a.md`。
- **边界**：digest/画像/对账提名/自动摄入（M3）、浏览页（M4）、排序乘子（等 dogfood 数据）未做；CLI 无 memory 子命令。M2/M3 预备事实见 M1 施工图 §6（已归档）。

## RAG 问答 `/v1/answer`（2026-07-14 改造为 Agentic）

fs 数据面知识库问答：LLM 经 **OpenAI function calling** 自主多轮调用 `search` / `read_file` 召回,再生成**带可验证引用**的答案。设计见 `docs/plans/veda-answer-agentic.md`(取代 07-10 的 one-shot 组装管线,退役理由在内)。

- **`veda-core/service/answer.rs`**：`AnswerService`(依赖 `ToolExecutor` + `LlmService` 两个 trait;`LiveTools` 为生产实现,包 SearchService+FsService;可选 `MemoryService` 为第二证据源,见记忆 M2a 节)。流程:用原始 query 预检索(route limit,默认 12)+ 团队域记忆检索(top-5)并列进首条 user 消息 → agentic loop:每轮 `chat_stream`(流式,tools 随带),LLM 回 tool_calls 则执行(search 固定 limit 6 / read_file 截 8000 字符可 offset 续读,path_prefix 硬约束)并以 role=tool 回填,回 content 即终答;≤`answer_max_tool_rounds`(默认 4)轮后强制收尾(不带 tools)。工具错误一律回填文本让 LLM 自愈。**Prompt 分层**:system = 内置知识库协议(工具用法/引用/防注入/拒答,不可覆盖)+ bot persona(请求 `prompt` 字段,空则 `DEFAULT_BOT_PROMPT`)。**Block 注册表**:search hit 按 (path,chunk) 去重编号,read_file 按 path 注册整文件块(citation `spans:[]` = 整文件);`[n]` 对齐生成 citations,零有效引用→citations 留空+`ungrounded`(不回填未引用块:检索恒返回 top-k,回填会把无关路径当出处外泄)。时间预算:loop 总 80s,单次 LLM 尝试 20s,剩余 <25s 不再开工具轮;重试规则=该次调用未向下游转发过 delta。`LlmService` trait:`summarize` + `chat_stream(messages,tools)`(旧 complete* 已删);`veda-pipeline/llm.rs` 含流式 tool_calls 分片拼装器。
- **`veda-server/routes/answer.rs`**：`POST /v1/answer(/stream)`(`AuthWorkspace`,fs only)。挂在 30s TimeoutLayer **之外**、自带 90s 兜底 deadline;per-workspace 并发信号量(`answer_concurrency` 默认 2,超出 429);query ≤1024 字符、`prompt` ≤4000 字符;SSE 事件 `delta`/`reset`(丢弃已积累 delta,罕见的说话后调工具轮)/`tool`(2026-07-15:工具执行前的进度提示 `{name,detail}`,detail=检索词/文件路径 ≤60 字符不含结果,可丢弃)/`final`(权威)/`error`。「没找到」不再是检索空提前返回,而是 LLM 输出固定话术(outcome=empty 按话术判定)。配置:`[llm]` 下 `answer_max_tool_rounds`(4,调 0≈退化 one-shot 应急旋钮)/`answer_max_output_tokens`(**4096**,2026-07-16 从 1024 上调:旧值被网关静默不执行,生产 p50 答案 ≈1.4k 字符,4096 才对得上真实分布)/`answer_concurrency`(2);`answer_max_context_tokens` 已删。指标:`veda_answer_request_seconds{outcome}`、`veda_answer_rounds`(loop 失控监控)等;hit_count=累计去重资料块数。
- **veda-tunnel 接入**：`[answer] enabled`(默认 true,改动需重启)优先走 **`/v1/answer/stream`**(SSE:delta 逐段 → tunnel ≥1s 节流刷新企微气泡,final 帧权威含 citations;`tool` 进度事件渲染为「🔍 正在检索:…」/「📄 正在查阅:…」状态行帧填补工具轮静默段,与 interim 共享节流;老 server 404/405 自动回落 `/v1/answer`),回复=答案正文+出处列表;false 回退纯检索直出。错误话术:501→「问答未启用」、429→「太频繁」、**`LLM_UNAVAILABLE`/`EMBEDDING_FAILED`→「上游 AI 模型服务暂时不可用（外部依赖故障，非知识库问题）」**(2026-07-30 aaeed55:上游故障不再甩锅知识库,one-shot/流式/纯检索三条路径统一,qa_log 记为独立 outcome `upstream_error`,console 有「上游故障」badge 与筛选)、`ANSWER_TIMEOUT`→保持通用「暂时不可用」(其 deadline 跨了检索,归因上游会误判)。per-bot `prompt`(veda_tunnel_bots 列)随请求透传。
- **验收**（真实 veda_it MySQL+测试 Milvus+airouter）：端到端带引用答案 3.5s；不编造（无关问题固定拒答）；400/501/kind-mismatch 负向全过。P1（SSE 流式、L1 全局题路径）、v1.5（db kind）见 plan §12。

## 平台网关面（AI Workbench / OnePaaS）

公司 AI 平台把 veda 作为存储底座的专用 surface，与原生 `vk_`/`wk_` 面并存：

- **控制面 `apps.rs`**（`/v1/workspace/{workspace}/...`）：`{workspace}` 是平台侧 workspace code（内部存 `app_id`），其下 veda 自己的 workspace 改叫 **project**（按 `id` 定位）。project/dataset/key 生命周期 CRUD + `GET /v1/my/projects`（当前网关用户的项目扁平列表，keyword 过滤 name/description，offset 分页 `page/size/order_by/order`）。**无 veda 凭证**——鉴权外置给平台网关。⚠️ 与原生面的一处语义差异：平台面有 `GET .../keys/{key_id}/token` 可**回读 `wk_` 明文**，而原生 `vk_` 面明文只在创建时显示一次；接入方评估凭证暴露面时要注意
- **数据面 `project_data.rs`**（`/v1/workspace/{workspace}/project/{id}/...`）：把 `wk_` 数据面（vectors upsert/search/query/delete + fs search/files/file/sql/grep + **fs 上传 `PUT /file?path=`/下载 `GET /file/content?path=`**，上传同 fs.rs 的 UTF-8/blob sniff 分流、下载带 RFC 5987 attachment 头）包装到网关 surface，前端不持 `wk_`。**读写都过外部 authz**（`authz_and_load`，2026-06-23 定）：数据面暴露实际内容，不依赖网关限路径，veda 独立验证用户在该 workspace 的权限。文件预览截断 256KB，二进制返回 `is_binary` 标识
- **`platform.rs`**：网关在 base64 `user` header 里传身份（`GatewayUser`，取 `name`/`displayName` 落 `creator`/`creator_name`），Cookie 透传给平台 authz/workspace-lookup API；直连（无 header）自动回退原生 key 鉴权。首次 `POST` 按 workspace code 自动开户
- **company envelope 中间件**：handler 返 veda `ApiResponse<T>`，中间件改写成公司规范（`Vec<_>` → `{data:[...], page,...}`；单对象 → bare object；create 返 200 非 201）
- **admin surface `admin.rs`**（`/admin/v1/...`，独立 `admin_token` 门控，fail-closed：未配 token 404）：跨租户只读 dashboard（workspaces/files/file 预览/vectors search/**fs 文档热度榜 `GET .../stats/docs`**——复用 `build_doc_stats`，db workspace 返回空板）+ db 向量写控制台（vectors upsert）；前端在 `web/src/admin.ts`（fs 详情页含「文档热度」区块：7/30/90 天窗口 + 读取/命中排序 + 口径 tooltip）

## veda-tunnel（外部 IM 接入）

独立进程 / crate（`crates/veda-tunnel`，二进制 `veda-tunnel`），把 veda 检索接入外部 IM。veda 数据面的标准 `wk_` 消费者，**veda-server 一行不改**。一期：企业微信智能机器人长连接（WSS `openws.work.weixin.qq.com`）+ 纯检索直出 + 管控面。

- **一 bot 一连接一 key**：每个企微机器人一条长连接 + 一个只读 `wk_`（绑一 workspace）。群 @提问 / 单聊 → 剥 `@` → `POST /v1/search`（Bearer `wk_`）→ 取 top-k `content`+`path` 拼 markdown → 长连接流式回（先 `finish:false` 占位吸收企微 5s 超时，检索完 `finish:true`）。
- **连接生命周期**（`wecom/conn.rs`）：`aibot_subscribe` 订阅 → 30s `ping` 心跳 → 断线/被踢（`aibot_event_callback` 的 `disconnected_event`）指数退避重连；msgid moka TTL 去重防 5s 重推双查；WS sink 由单写循环独占，读循环 / 心跳 / handler 都经 mpsc 投帧。
- **bot 管理（共享 MySQL 表，三入口）**：bot 配置存 MySQL（`veda_tunnel_bots` 表，生产与 veda-server 同库，`store.rs` bootstrap + information_schema 列迁移；`config.toml` 的 `[[wecom.bot]]` 仅首次 seed）。三个写入口收敛到同一张表：① console UI `#/admin/tunnel`（经 nginx `/tunnel/v1/*` → tunnel `:9110/admin/*`，即时生效）② tunnel admin CRUD API（同上）③ **veda-server 平台 API**（`/v1/workspace/{ws}/project/{id}/tunnel/bots`，AI 工作台专用：直写共享表 + 自动 mint/revoke 只读 `wk_`，`routes/tunnel_bots.rs` + `tunnel_bots.rs`，进程间零 RPC）。tunnel 侧 **30s store 轮询 reconcile**（`reconcile::plan` 纯函数 diff：新增 spawn / 变更 respawn / 删除 stop）收敛平台写入，并把 `conn_state` 心跳写回表（仅变化时 UPDATE，`updated_at=updated_at` 保配置时间戳），平台 GET 可见在线状态。
- **问答质量遥测（qa_log）**：每次问答落 `veda_tunnel_qa_log`（query/提问人 user_id/答案原文/outcome 七分类（answered / no_context / ungrounded / error / disabled / throttled / upstream_error）/延迟/引用数/**tool_trace 检索过程**——流式 `tool` 事件按序收集为 JSON 数组 `[{tool,detail}]`（检索词/读过的文件），节流外采集不丢步；两侧 DDL 副本 2026-07-16 起带 tool_trace 列迁移，best-effort 不阻塞回复）；stream 首帧带 `feedback.id` 激活企微点赞点踩，`feedback_event` 回流 `veda_tunnel_qa_feedback`（同人改评价替换）。outcome=no_context 按 server 拒答话术前缀判定（语义检索恒有 top-k，hit 数不可用）——该清单即知识库内容缺口。admin `/admin/stats`+`/admin/qa-log`，console tunnel 页有统计卡+bad case 明细（时间列带提问人，明细行可展开「答案」与「过程」）。**AI 工作台读**（2026-07-15）：veda-server apps surface 增 `GET .../tunnel/qa/stats`（days 1–90 outcome 分布+赞踩）与 `.../tunnel/qa/logs`（分页明细，`outcome`/`down_voted`/`bot_id` 过滤）；qa 两表随 `TunnelBotStore` 幂等 bootstrap（可先于 tunnel 起在新库），**租户隔离**=先查 project 名下 bot_id 集合再 `bot_id IN(...)` 绑定约束，跨 project 的 bot_id 一律 NOT_FOUND，读过外部 authz（明细含用户原文）；`routes/tunnel_bots.rs`+`tunnel_bots.rs`，集成测试 `tests/tunnel_qa_test.rs` 真 MySQL 验隔离。计划：`docs/archive/plans/veda-tunnel-qa-log.md`。
- **管控面**（`admin.rs`，默认 `127.0.0.1:9100`，fail-closed `admin_token`：未配 404 / 错 401）：`GET /admin/bots`（配置+状态，secret 不返回、`veda_key` 脱敏）、`POST/PUT/DELETE /admin/bots[/{id}]`（CRUD，编辑时 secret/key 留空=保留）、`POST /admin/bots/{id}/reconnect`、`POST /admin/reload`（从 MySQL 全量重载）、`GET /healthz`。
- **单实例 + adapter 结构**：企微「新连接踢旧」约束决定全局单实例持 bot 连接（多实例选主划到未来）。`wecom/` 是第一个 adapter，`config`/`registry`/`veda`/`admin`/`store` 通用，未来 `feishu/` 平级新增。依赖复用 workspace，新增 `tokio-tungstenite`（rustls，同 reqwest TLS 栈）+ `sqlx`（MySQL bot store，同 veda-store 客户端）。

**状态**：生产运行中——2026-07-13 迁至专用生产机 **10.79.52.95**（`tdchw-veda-tunnel-1`，systemd `veda-tunnel.service`，连生产 MySQL `veda` 库 + `.85:3000` 内网直连，admin `0.0.0.0:9110` 用生产 token；nginx 两入口 `/tunnel/v1` 反代已切 .95，.161 旧实例 stop+disable）。真机联调（2026-07-09）+ `/v1/answer` RAG 链路（2026-07-10）+ 平台 API/轮询 reconcile（2026-07-13，集成测试全 CRUD+key mint/revoke 不变量真 MySQL 验证）。部署 runbook：`docs/deploy-tunnel.md`；设计：`docs/plans/veda-tunnel-plan.md`；平台契约：APIDoc `docs/veda/tunnel-bot-api.md`。

## Workspace kinds: fs vs db

每个 workspace 在创建时锁定 `kind`，决定它服务哪条 API 通道：

| `kind` | 数据载体 | API 通道 | 用途 |
|---|---|---|---|
| `fs`（默认） | `veda_dentries / veda_files / veda_file_contents / veda_file_blobs / veda_file_extracts / veda_file_chunks / veda_summaries` (MySQL) + `veda_chunks / veda_summaries` (Milvus) + `veda_collection_schemas` (structured collections) | `/v1/fs/*`（含 `fs-copy`/`fs-rename`/`fs-mkdir`）, `/v1/search`, `/v1/grep`, `/v1/sql`, `/v1/abstract`, `/v1/overview`, `/v1/collections/*`, `/v1/answer`, `/v1/events`, `/mcp`, FUSE | 文件知识库（既有能力） |
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

完整设计：`docs/archive/vectors-merge-plan.md`（鉴权/app_id/write_mode 等章节已被演进推翻，见其头部 2026-06-10 注记）；历史待办已归档：`docs/archive/vectors-merge-backlog.md`（未兑现尾巴并入 `docs/plans/db-workspace-followups.md`）。

### 客户端 SDK

- **Java SDK**（`sdk/java`，独立 Maven 项目，不在 Cargo workspace）：db 数据面 4 端点（upsert/search/query/delete）的 Java 8 封装。Jackson + OkHttp，fluent filter builder，`error_code`→类型化异常（未知码归 `UNKNOWN`），幂等感知重试（id-less upsert 不自动重试），响应前向兼容（`ignoreUnknown`）。单测绿；真实 server 契约测试走 `mvn -P integration verify`（发版 gate，非 CI）。已发布 `csoss.veda:veda-sdk-java:0.0.1-SNAPSHOT` 到内部 ddxq Nexus（2026-06-04）。设计：`docs/archive/plans/java-sdk-db-plan.md`。**⚠ 待适配**：db 数据面 2026-06 已从 `vk_` 改 `wk_`、请求体去掉 `workspace_id`，SDK 的 `apiKey`→`workspaceKey` 改造 + `write_mode` + e2e 重测尚未做（见 docs/todos.md）。
- **Python 示例**：`examples/python_pinecone_demo.py`（无 SDK，裸 HTTP）。

## 数据保留（retention）

后台 sweep 任务（`[retention]`，默认开启，`interval_secs` 默认一天、下限 60s）定期删除超期行，避免两张只增表无限膨胀：`veda_fs_events`（默认留 14 天——SSE cursor 超出该窗口的客户端会收到 `410`，需重新订阅）与 `veda_outbox` 终态行（`completed`/`dead`，默认留 1 天）。计数指标 `veda_fs_events_retention_swept_total` / `veda_outbox_retention_swept_total`，全部键支持 `VEDA_RETENTION_*` 覆盖。

## 部署形态

- **单进程**：server 与 outbox worker 同进程，无 HA。`deploy/`（Dockerfile、docker-compose、Prometheus/Grafana）用于本地与单机栈；生产走 `scripts/deploy/` 的 systemd 单元 + `docs/deploy.md` / `docs/deploy-runbook.md`。
- **systemd socket activation**：listener 由 socket unit 持有（`Backlog=4096`），swap 二进制后 restart 即平滑——连接在 backlog 排队而不是被拒。此时配置里的 `listen` 被忽略。
- **`drain_secs`**：SIGTERM 后继续服务的秒数，期间 `/v1/ready` 返 503 "draining" 让 LB 先摘节点。默认 0；**单节点必须留 0**（没有接管者，drain 窗口纯粹是停机时间），扩容到多节点时才配。
- **`TimeoutStopSec` 要盖住最慢的一次 embedding 批**（生产用 120s），否则优雅关闭会被 SIGKILL 打断。
- **配置模板**：`config/server.toml.example` 列全部键、默认值与 `VEDA_*` 覆盖；必填只有 `[mysql] [milvus] [embedding]`，生产另需 `metrics_token`（未配 `/v1/metrics` 与 `/admin/v1/reconcile/*` 均 404）、`admin_token`（未配则 `/admin/v1/workspaces*` 只读 dashboard 404，console 看板失效；**不覆盖** `/admin/v1/tokens`（走 `vk_`）与 reconcile（走 `metrics_token`），tunnel 管控用的是 tunnel 自己配置里的 `admin_token`）、`allowed_origins`、`[otlp]`。
- **二进制 argv**：`veda-server [config.toml]`——单个位置参数，省略则默认 `config/server.toml`；只认 `--help` 和 `--version`（后者往 stdout 打 `veda-server <crate 版本>`，退出码 0；传其他 `--flag` 直接报错退出）。**没有版本端点**：`GET /capabilities` 只报 `summary_enabled`；`POST /mcp` 的 `initialize` 里有 `serverInfo.version`，但那是 **crate 版本号，不等于 build**——节点上跑未发版 commit 时它照样显示上一个 tag（.85 就出现过）。核对线上到底是哪个 build 要比二进制 sha256，见 `docs/deploy-runbook.md`。

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
- CLI/FUSE 对**真实 server** 的端到端测试（`crates/veda-fuse/tests/binary_roundtrip.rs` 已有 mock HTTP 的二进制往返集成测试，缺的是挂真 server 那一层）、K8s Helm chart
- OTLP trace 二期（协议事实见 `docs/archive/plans/observability-otlp-plan.md` §0）

## 关键设计决策

原始设计提案已归档（`docs/archive/design/design.md`——其中凭证体系 / reconciler / API 面 / 部署形态均已被演进推翻，**勿当现状读**，现状以本文件为准）。摘要：

- MySQL = control plane (元数据、认证、outbox)
- Milvus = data plane (向量搜索、structured collection 数据)
- 文件分层存储：UTF-8 文本 ≤256KB inline，>256KB chunked；非 UTF-8 存 `veda_file_blobs`（LONGBLOB，magic-byte 判 MIME）；可提取 blob（pdf/word）的派生全文存 `veda_file_extracts`（file_id PK + source_sha256，纯缓存可整表重建，与 blob 同事务删除 + 读侧 sha 校验双保险防 stale）
- Dedup 只有两条路径，**没有跨路径的内容寻址**：同路径同 checksum 的写入是 no-op（`If-None-Match: "<sha256>"` / 服务端 checksum 比对，返回 `content_unchanged`）；copy 走 ref_count 共享同一份内容（COW，写时才分家）。两个不同路径写相同内容 = 存两份
- Outbox pattern 实现最终一致性。文件写入与其 ChunkSync/SummarySync 入队在**同一 MySQL 事务**提交，写路径不会漂移。机制：10 分钟租约 + `FOR UPDATE SKIP LOCKED` 抢占，失败退避 `30·2^n`（上限 1h），超 `max_retries` 转 `dead`。**2026-07 单 pod 简化删除了 `lease_owner` fencing 列**——终态转换现在只 fence `status='processing'`，靠内容哈希水印保证罕见的重复执行幂等；`lease_until` 到期接管（崩溃恢复）不变。这把「单写者」假设变成显式约束：**绝不能让两个 server 进程指向同一个数据库**（本地开发和集成测试也算），扩容到多 pod 前必须先把 fencing 加回来。该迁移只能停机升级，不兼容滚动发布。残余漂移来源只有死信任务（`veda_outbox_dead_total` + `veda_outbox_depth{status}` 暴露，告警在 Monitor 平台配）和 Milvus 侧数据丢失（磁盘/运维/破坏式迁移）。**不再有 6h 后台 reconcile loop**；改为按需 `POST /admin/v1/reconcile/{workspace_id}?dry_run=`（ops `metrics_token` 鉴权，默认 dry_run=true 只报告，失败响亮返回 500）
- Account → Workspace 多租户；控制面 `vk_`、数据面 `wk_`，纯 key 校验（JWT 已移除）
- VedaError::Storage 使用 String（而非 anyhow::Error）避免 lib crate 兼容问题
- **三层信息模型 (Tiered Context Loading)**：
  - L0 Abstract (~100 tokens)：文件/目录的一句话摘要，存入 Milvus 做向量搜索
  - L1 Overview (~2k tokens)：结构化概览，按需从 MySQL 批量加载。所有 summary LLM 调用（L0/L1/目录聚合）共用 `max_summary_tokens` 预算（默认 8192）——预算是防截断保险丝而非输出长度控制；推理模型的思考 token 在部分网关后端计入 max_tokens，预算过紧会在思考阶段耗尽产出空 content（2026-07 生产事故）。`[llm] summary_disable_thinking`（默认 false，TOML-only）会给**摘要**请求带 `enable_thinking: false`——公司 airouter + deepseek-v4-flash 实测生效（思考 token 占比 ~87% 归零、延迟约减半、上述空 content 机理从源头消失）；非标准参数，OpenAI 官方 API 对未知顶层参数报 400，只在确认支持的网关上开。`/v1/answer` 的流式调用**任何配置下都不带该字段**（保留思考是刻意决策）。8192 预算与空 content 重试护栏保留，兜没开该配置的后端 / 配置漂移 / 上游偶发空响应。`[llm] summary_fallback_model`（默认 None，TOML-only）是**容量**降级：主模型重试被 429 耗尽后换该模型再发一次同形请求（`enable_thinking` 照旧跟随 summary_disable_thinking）——前提是 airouter 配额按模型隔离（2026-08-04 实测：同 key 同一 429 窗口内 qwen-flash / deepseek-v3.1 / v4-pro 全部成功）。5xx / 空 content **不触发**（后端有病，换模型只是加倍施压）；降级也失败则回落 outbox 60/120s
  - L2 Full (原文 chunk)：现有 chunk 搜索
  - 写入流程：写入事务里**同时入队 ChunkSync 与 SummarySync**（两者互不阻塞——ChunkSync 不再串行入队 SummarySync，embedding 失败不再拖累 L0/L1 生成，父目录聚合也不会漏掉 chunk_sync 死在 400 的文件）→ SummarySync 完成后入队 DirSummarySync（自底向上聚合，含去重防抖）
  - 文件和目录的 L0 均写入 Milvus `veda_summaries` 集合（Abstract 搜索可命中目录）
  - SummarySync / DirSummarySync 入队前检查去重，避免重复 LLM 调用
  - 搜索 API：`detail_level` 参数控制返回粒度，Abstract/Overview/Full 三级
  - LLM 配置可选，未配置时 summary 功能自动禁用
