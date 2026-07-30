# Veda 学习实战手册

> 目标读者：想从「大概了解」升级到「能独立维护迭代」的 owner（Joe）。
> 方法论来自 Thariq Shihipar 的 *A Field Guide to Fable: Finding Your Unknowns*，
> 反向用于学习——系统性地把你的 unknown-unknown 搬到明面上。
> 本文内容由三个探索 agent 逐行读代码产出，非从 ARCHITECTURE.md 推断。

---

## 0. 怎么用这份文档

你的 unknown 分四象限，最危险的是**你没意识到自己不知道**的那类。vibe coding 项目尤其如此：代码是 AI 写的决定，你被动接受了却说不出来。这份文档专收 **ARCHITECTURE.md 猜不到、但改代码时不知道就会踩坑** 的实现细节。

**每个维度走同一个四步循环：**
1. 先凭记忆写 10-15 行「我理解的 X 如何工作」（不看代码，写不出来的地方就是发现）
2. 对照本文档 + 代码找 diff
3. 读带 `文件:行号` 的流程图，开着代码跳转验证
4. 做 `quiz.md` 对应题目，**全对才进下一维度**

**维度顺序（依赖递增）：** 契约 → 数据 → 写路径+outbox → 鉴权 → 搜索 → FUSE → SQL/CLI

**毕业标准：** 能独立评估 `docs/todos.md` 里每一条方案是否合理。

**可信度分级：**
- ✅**已交叉核实** = 我（或你）亲自读源码确认过
- ○**agent 报告** = 探索 agent 逐行读码所得、抽查未见矛盾，学到时请顺手验证行号（代码会漂移）

---

## 1. 三条核心链路（带指针）

### 1.1 文件写入链路 `PUT /v1/fs/{path}`

```
HTTP PUT /v1/fs/{path}          routes/fs.rs:96 write_file()
│  ├─ body 限 51MiB（故意比 50MB 配额大 1MB，让配额错误由 service 层报）
│  ├─ std::str::from_utf8 嗅探：成功→text 路径，失败→blob 路径（不是看 Content-Type！）
│  └─ If-Match（revision 乐观锁）/ If-None-Match（sha256 幂等）
▼
FsService::write_file           service/fs.rs:275
│  ├─ 配额 50MB；compute_write_meta 在 spawn_blocking 上算 SHA256 + 分块
│  │    split_and_hash：按 \n 对齐切 256KB；≤256KB→Inline，>256KB→Chunked
│  └─ retry_on_deadlock 包裹（死锁重试 3 次）
▼
write_file_once（事务内）        service/fs.rs:360
│  ├─ path::normalize：NFC、解析 ./.. 、拒绝 ':' 和控制字符
│  ├─ reject_reserved_basename：.abstract / .overview 保留给 FUSE sidecar
│  ├─ 同 path 已存在：
│  │    checksum 相同 → no-op 提前返回（不 bump revision、不发 outbox）
│  │    revision 不匹配 → 412 PreconditionFailed
│  │    否则 finalize_full_rewrite：
│  │       ref_count>1 → COW（新建 file 记录，旧 file ref_count-1）
│  │       ref_count=1 → 原地改 + 把 last_embedded_content_hash 置 NULL
│  ├─ 写表：veda_dentries + veda_files + (file_contents|file_chunks|file_blobs)
│  │    chunks 50 条一批插入（防 max_allowed_packet）
│  ├─ enqueue_index_outbox：Text→ChunkSync + SummarySync（两个独立事件）
│  │                        Pdf→ExtractSync；Image/Binary→不入队
│  └─ 同事务写 veda_fs_events + COMMIT（写与 outbox 原子提交，不会漂移）
▼
Worker 消费                     worker.rs:80 run()
│  ├─ claim：FOR UPDATE SKIP LOCKED，抢 pending 或 lease 过期的 processing
│  │         （顺手 retry_count+1，超限直接 dead）
│  └─ process_batch：for_each_concurrent 并发 + 180s lease renewer + catch_unwind
▼
handle_chunk_sync               worker.rs:289
│  ├─ 水印检查：last_embedded_content_hash == checksum 且非 force_reembed → 跳过
│  ├─ is_binary_content（NUL 字节检测）→ 跳过（含 NUL 的 UTF-8 存为文本但永不 embed）
│  ├─ semantic_chunk：Markdown 标题切分 + 字符权重滑窗
│  └─ embed_and_watermark，四步顺序是幂等关键：
│       ① embed（64/批）→ ② Milvus upsert_chunks_only
│       → ③ delete_chunks_above 清旧尾块 → ④ 最后才写水印
│       Milvus chunk PK = "{file_id}_{chunk_index}"
▼
成功→complete（栅栏=status='processing'；lease_owner fencing 已于 2026-07 删除——单 pod 简化，重复执行由内容水印兜底）
失败→fail：retry_count+1，退避 30*2^n 秒（上限 3600），超限→status='dead'（`veda_outbox_dead_total` 已上报 Monitor；告警规则是否配好需平台侧核实——06-12 的 8192 死信当时就是没人看见）
```

### 1.2 搜索链路 `POST /v1/search`

```
routes/search.rs:29 search()（limit 上限 100）
▼
SearchService::search   service/search.rs:27，按 detail_level 分流：
│
├─ abstract → search_abstract：⚠️ 强制 Semantic，忽略请求里的 mode
│    embed(query) → Milvus veda_summaries 集合 ANN → path_prefix 在 Rust 侧过滤
│
├─ overview → = abstract 结果 + 从 MySQL veda_summaries 补 l1_overview
│
└─ full → search_full：
     Fulltext → BM25 over sparse_vector（jieba 分词）
     Semantic → ANN over dense vector（COSINE）
     Hybrid  → Milvus /hybrid_search（dense ANN + sparse BM25），RRF 融合在 Milvus 端（k=60）
               ⚠️ 调用失败 → 静默 fallback 到纯 ANN（只 warn，用户无感）
     全部 consistencyLevel=Strong
▼
resolve_paths：file_id 批量反查 path
⚠️ 目录摘要命中的 "file_id" 实为 dentry_id → 查不到 path → path=None
   → 带 path_prefix 时目录命中被静默丢弃
```

### 1.3 一致性 / Reconcile 链路

```
POST /admin/v1/reconcile/{workspace_id}   routes/reconcile.rs:34
│  ⚠️ 鉴权用 metrics_token（不是 admin_token/wk_）；未授权返回 404
│  ⚠️ dry_run 默认 true，真修复要 ?dry_run=false
▼
Reconciler   reconciler.rs
├─ reconcile_chunks：MySQL file_ids ⟷ Milvus chunk file_ids 对账
│    Milvus 缺 → 入队 ChunkSync，payload 带 force_reembed=true（绕过水印）
│    Milvus 多（孤儿）→ 复查 MySQL + 检查 pending ChunkSync 后立即删
│    （grace counter 已于 2026-07 删除——生产恒 grace_passes=0，本就是死机制）
└─ reconcile_summaries：同理对账摘要
⚠️ 大 workspace（>16383 行）reconcile 直接 500（offset 翻页天花板，review 已知）
```

---

## 2. 文档 vs 代码漂移（最危险，✅ 已交叉核实）

这三条都和 ARCHITECTURE.md 给你的印象不符，是典型 unknown-unknown：

1. ✅ **「content-addressed dedup」并未跨文件启用。** `find_file_by_checksum` 只有 trait（`veda-core/src/store.rs:116`）+ MySQL 实现（`veda-store/src/mysql.rs:1051`），**生产零调用，只有测试用**。真实 dedup 只有两种：同路径同 checksum 的 no-op、copy 时 ref_count 共享。两个不同路径写相同内容 = 存两份。ARCHITECTURE.md 那句 "Content-addressed dedup (SHA256)" 会误导。

2. ✅ **hybrid 搜索失败静默降级为纯语义。** `veda-store/src/milvus.rs:1124-1127` 失败只 `warn!` 返回 `None`，`milvus.rs:1258` fallback 到 `ann_search`。排查「hybrid 效果差」必须先翻日志确认没在降级。（注：milvus.rs:866 附近有另一条注释说 ANN 那条路径「无 fallback」，别混淆——降级只发生在 hybrid→ANN 这一层。）

3. ✅ **`VEDA_PLATFORM_BASE` 没配 = 平台面鉴权整体 fail-open。** `veda-server/src/platform.rs:112-124` 注释原文："When unset, external authz is **not enforced**"。这个 env **不在 config.rs / ServerConfig 里**，每请求现读、启动不告警。新节点忘配 = `/v1/workspace/{ws}/*` 平台面（含向量增删、文件读、SQL）静默无鉴权。这是全项目最危险的单点配置。

---

## 3. 非显然实现细节（按维度，○ agent 报告，学到时验证行号）

### 写入 / 存储
- text/blob 分流靠 UTF-8 嗅探，客户端传的 MIME 只影响记录不影响分流。`routes/fs.rs:96`
- 二进制检测在 **worker 侧**（NUL 字节），不是写入时；含 NUL 的 UTF-8 会存为文本但永不 embed。
- `last_embedded_content_hash` 水印生命周期：重写时置 NULL（`mysql.rs:1669`）→ worker embed 成功后设为 checksum（`mysql.rs:1262`）→ reconciler 用 `force_reembed=true` 绕过。**手动改 MySQL 内容不清水印 = Milvus 永不更新**。
- 递归 delete/rename 上限 10 万 entry，超限整个事务 QuotaExceeded 失败（全有全无，无分批 API）。`service/fs.rs:24`
- rename/move **不动 Milvus**（向量 PK 基于 file_id 而非 path），但会给新旧两个父目录都入队 DirSummarySync。
- append 增量只在「文件已 chunked 且 ref_count==1」时只重切尾块，否则回退全量重写。

### 一致性 / outbox / worker
- **两套 max_retries**：FsService 写入入队=5（`service/fs.rs:1815`），worker/reconciler 的 enqueue_dedup=3（`outbox.rs:47`）。改重试策略要改两处。
- **ChunkSync 和 SummarySync 写入时独立入队**（不是链式）。历史坑：曾由 ChunkSync 链式触发 SummarySync，导致 embedding 400（超 token）时摘要永不生成——别改回去。
- outbox 去重只针对 `pending`，**故意不针对 `processing`**：否则处理旧快照的任务会吞掉新内容事件。水印兜底第二个事件短路。`mysql.rs:1882`
- `embed_and_watermark` 四步顺序（upsert→删尾块→写水印）是精心设计的崩溃安全；把水印挪前面，失败重试会被水印挡住，Milvus 永久缺数据。
- claim 时 lease 过期的任务直接 retry_count+1，**可能不经 fail() 就进 dead**（无退避、无 error 记录）。`mysql.rs:1977`
- complete/fail 丢 lease 时只打日志（"outbox complete dropped"）——**重复执行是常态**，靠 Milvus upsert 幂等兜底。审计时要知道这点。
- LLM 未配置时，ChunkSync 完成后**主动删除该文件既有摘要**（防过期 L0/L1 被搜到）。`worker.rs:203`
- 摘要防抖两级：DirSummarySync 入队设 `available_at=now+30s`，且 5min burst window 内靠 `has_pending_event` 去重。批量写 100 文件不会触发 100 次目录聚合。
- **dead letter 无出口**：只有 metrics 可见，无告警、无 replay 端点；唯一救回是 admin reconcile（且只覆盖「Milvus 缺数据」类漂移）。

### 搜索
- `detail_level=abstract` + `mode=fulltext` → mode 被静默忽略走 semantic（摘要集合无 BM25 索引），不报错。`search.rs:52`
- path_prefix 在 Rust 侧过滤，over-fetch 3x 后过滤；前缀下稀疏时结果会少于 limit 甚至为空——不是「没数据」是「候选里没匹配前缀的」。
- 目录摘要命中在 path_prefix 下静默消失（dentry_id ≠ file_id，path 解析不到被丢）。

### 鉴权 / 多租户
- **`/admin/v1` 前缀下三种互不相干的门**：`workspaces*`→admin_token、`tokens*`→vk_、`reconcile`→metrics_token。按路径猜权限必错。
- **key 存储不一致**：native 面 vk_/wk_ 只存 SHA-256 hash；但 apps 面 mint 的 wk_ **明文**存 `veda_workspace_keys.token` 列供 getToken 取回。DB dump 爆炸半径不同。`mysql.rs:3098,3150`
- wk_ 校验**不查 workspace.status**，靠 `delete_workspace` 事务级联 revoke key。手工 SQL 把 workspace 置 archived 而不 revoke key，数据面依然开着。
- 平台**管理面 GET**（list/get project、list keys masked、list datasets）**不调外部 authz**，只有 mutating + getToken 调；**数据面每个操作都调**（2026-06-23 决策）。
- 开放注册：`POST /v1/accounts` 和 `/v1/accounts/anonymous` 无鉴权无限量，代码里无任何 rate limit。
- metrics_token 同时守 reconcile，`?dry_run=false` 会真删 orphan——监控 scrape token 实际有 mutating 权力。

### 配置 / 运维 / 发版
- **可选段不配即静默禁用**：llm（→summary 全禁、abstract/overview 返 501）、metrics_token（→/v1/metrics 和 reconcile 都 404）、admin_token（→dashboard 404）、otlp（→不推送）。
- `env_parse` 对格式错误的数字 env **静默忽略**保留 TOML 值（`VEDA_MYSQL_MAX_CONNECTIONS=abc` 不报错）。`config.rs:357`
- `embedding.batch_size` 默认 100，但 DashScope/百炼上限 10，不改会报错。
- socket activation 下 `listen` 配置被忽略，真实端口在 `veda-server.socket` 的 `ListenStream=3000`。
- **发版铁律**：server 纯手工（唯一权威 `docs/deploy-runbook.md`，skill 只是护栏），在 .89（glibc 2.34）单次 build 复用三节点；swap 必须 `mv` 原子换 binary **先于** `systemctl restart`（socket activation 会用磁盘上的 binary 拉起）。
- schema 无版本化迁移，靠启动时 `migrate()` 幂等 ALTER；服务账号缺 ALTER/INDEX/UPDATE 权限 = 起不来。
- **回滚认「代码是否含 blob」不认 version 号**（测试节点有显示 0.1.14 却含 blob 代码的构建）；回滚前必跑 blob gate SQL。
- `/healthz` 纯活性不碰 DB；`/v1/ready` 才 ping MySQL/Milvus。liveness probe 打错端点会在 DB 抖动时无意义重启。
- `install.sh` 由 server 编译期 `include_str!` 嵌入——改安装脚本必须重发 server。

### FUSE（并发最重，改动最危险）
- **writeback 四状态**：无 entry / LocalOnly（server 没见过）/ Dirty（server 有但本地更新）/ Clean，外加独立 tombstone 集合。全局单调 `seq` 是竞态判定基础。
- create() **完全不打 server**（defer 到第一次真正 Touch）；vim swap 全生命周期（create-write-unlink）产生 **0 个 server 调用**。
- commit 合并靠 **deadline token 不靠 seq**（`mark_committed` 不 bump seq，write→flush→release 三连同 seq 要合成一个 PUT）。
- **macOS fork 红线**：父进程 fork 前碰 reqwest（内部起 tokio 线程）→ 子进程 SIGILL。所以 daemon 的 HTTP client 和 preflight 都在 fork 后子进程做，错误经 readiness pipe 回传。CommitQueue 也因此坚持 `std::thread` 不用 tokio。
- per-file 超 10MB → 静默降级同步写（用户不见 EFBIG）；total 超 50MB → ENOSPC。
- 进程 crash 丢 debounce 窗口内的写（无持久化，单用户 alpha 接受）。
- veda-fuse **依赖 veda-cli 作为 lib**（复用 config 加载）——CLI 重构会连带破坏 fuse 编译。
- veda-fuse crate version 硬编码 "0.1.0" 且无 `--version`——排障别靠二进制自报版本。

### SQL / CLI
- SQL **每查询新建 SessionContext** + 256MB 内存池 + 30s 超时；`SQLOptions` 无条件禁 DDL/DML（防 `COPY TO`/`CREATE EXTERNAL TABLE` 读写宿主机）；`read_only`（来自 wk_ 权限）只管写 UDF → 403。
- **平台网关的 SQL 永远 read_only=true**。`project_data.rs:235`
- UDTF 的 `call()` 是**同步、在 SQL 计划期执行**：`search('q')` 的 embedding+Milvus 查询发生在 planning 阶段并整表物化进 MemTable。
- 跨 workspace 不可达：workspace_id 全部来自认证层注入，表和 UDF ctx 都按它构造。
- DataFusion pin 在 "53"/arrow 58，代码耦合 53.x API 形状——升级是全 crate 机械改写 + sql_test 全量回归。
- CLI↔server **无版本握手**：binary cp/cat 需 server≥0.1.15（旧 server 返 400）；契约靠 ApiResponse envelope + `/capabilities` 只增字段 + SSE 错误体 shape 冻结。

---

## 4. 改动高危区 top（改这些之前先做对应 quiz）

来自 FUSE/SQL agent 的「最容易改出 bug」清单 + review 的 P0：

1. **`commit_queue.rs::fire()` 结果侧协议**：PUT 后重锁→is_current→三分支（提交/什么都不做/追 DELETE）。把「仅 entry_gone」当删除条件会误删新内容；漏 tombstone 追杀会把已删文件泄漏回 server。
2. **`fs.rs::destroy()` 双 flush 防护**：shadow 已 drain 的 path 其 legacy buf 是空的，去掉「在 shadow 里就跳过 flush_handle」的过滤会在 unmount 时 PUT 空体覆盖刚提交的数据。
3. **outbox 租约 fencing 的来回**（review A-3 → 2026-07 简化）：当年 `complete`/`fail` 连 `AND status='processing'` 都没有，慢任务超 10min 租约被双 claim 会写坏状态；A-3 修了 status 条件并加了 owner(host:pid) fencing token + 心跳。**2026-07 按「单 pod 不上多 pod 并发保护」把 owner 栅栏删回**——现行形态 = `status='processing'` 条件 + lease_until 心跳续租,重复执行由内容水印幂等兜底。前提是**同一 MySQL 库只有一个 server 进程**(本地 dev 与集成测试别同时指 veda_it)。见 `docs/reviews/review-2026-06-10-154815.md` A-3。
4. **outbox 毒丸**（review A-4）：一条解码失败的行让 claim 事务回滚，`ORDER BY id ASC` 让它每轮最先被选 → 整个异步队列停摆。修复：解码失败标 dead 并 continue。
5. **InodeTable 双分配器**（local ino 用 `1<<63` 高位段，与 server ino 互踢）+ setattr-truncate 的 adopt-before-lock + readdir 三层 overlay 合并。

---

## 5. 起手式建议

按依赖顺序，前四个维度（契约/数据/写路径/鉴权）大概 4-6 个晚上能过完，FUSE 和 SQL 等真要动它们时再补。

**毕业项目（真正的 DoD）**：学完 outbox 维度后，亲手做 review #3 的地板修复（给 `complete`/`fail` 加 `AND status='processing'`）——几行，但你得能说清它防住了什么、没防住什么。之后挑 todos 里一条中等的（如 Java SDK 适配 wk_）独立评估方案、让 agent 实现、你用 quiz 验收。

相关文档：`ARCHITECTURE.md`（现状总览）、`docs/reviews/review-2026-06-10-154815.md`（现成的盲区清单）、`docs/todos.md`（毕业标准来源）、`docs/deploy-runbook.md`（运维唯一权威）。
