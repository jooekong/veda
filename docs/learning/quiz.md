# Veda 自测题库

> 按维度分组，每维度过关标准：**全对才进下一维度**。
> 答案要点和验证位置见文末，先答再对。答错的回 `field-guide.md` 对应章节 + 开代码读。
> 题目来自三个探索 agent 逐行读码后设计的检验点。

---

## 维度 A：写入 / 存储（先过这关）

**A1.** 文件 A 被 copy 成 B 后，再向 A 写入新内容，数据库层面发生什么？

**A2.** 客户端 `PUT` 一个 `.png`，但 Content-Type 头写成 `text/plain`，会走 text 还是 blob 路径？为什么？

**A3.** 你手动用 SQL 改了 `veda_file_contents` 里某文件的内容，为什么搜索永远搜不到新内容？怎么修？

**A4.** 一次 `rm -rf` 删 12 万个文件的目录树，会发生什么？

---

## 维度 B：一致性 / outbox / worker（核心）

**B1.** Milvus 宕机 3 天期间用户持续写入，恢复后怎么把索引补齐？期间写入会失败吗？

**B2.** worker 处理任务到一半进程被 `kill -9`，这个任务的命运是什么？

**B3.** 为什么 SummarySync 不由 ChunkSync 成功后链式触发？改回链式会坏什么？

**B4.** 5 秒内对同一文件写 10 次，产生几个 outbox 任务、几次实际 embedding？

**B5.** `embed_and_watermark` 的四步顺序是什么？把「写水印」挪到第一步会坏什么？

**B6.**（毕业题）outbox 租约模型的头号缺陷是什么？「地板修复」改哪里、几行？为什么它只是地板不是真解？

---



## 维度 C：搜索

**C1.** 请求 `detail_level=abstract` + `mode=fulltext`，实际怎么执行？会报错吗？

**C2.** 搜索带 `path_prefix=/docs/`、`limit=10`，却只返回 2 条，可能是哪两种原因？

**C3.** hybrid 搜索「效果时好时坏」，排查第一步该看什么？为什么 API 层看不出来？

---



## 维度 D：鉴权 / 多租户

**D1.** 生产 curl `/v1/metrics` 返回 404，可能有哪几种原因？

**D2.** 一个 wk_ 的明文在系统里哪几处可能被拿到？native 面和 apps 面有何不同？

**D3.** `/admin/v1/workspaces`、`/admin/v1/tokens`、`/admin/v1/reconcile` 三条路由分别用哪种鉴权？

**D4.** 运维手工把某 workspace 在 MySQL 里置成 archived，但没 revoke 它的 key，数据面会怎样？为什么？

**D5.**（危险题）新部署一个节点，忘了配 `VEDA_PLATFORM_BASE`，平台面 `/v1/workspace/{ws}/`* 会怎样？启动日志有告警吗？

---



## 维度 E：配置 / 运维 / 发版

**E1.** 部署时为什么必须先 `mv` 换 binary 再 `systemctl restart`？顺序反了会怎样？

**E2.** 升级后服务起不来，日志报 `ALTER command denied`，根因是什么？

**E3.** 回滚到 tag 0.1.14 前必须查什么？为什么不能只看版本号？

**E4.** LLM 段不配，用户可见的症状有哪些？（至少 3 个）

**E5.** `VEDA_MYSQL_MAX_CONNECTIONS=abc`（拼错）启动会怎样？

---



## 维度 F：FUSE（要动它时再过）

**F1.** vim 写 swap 再 `:wq` 删除，writeback 下 server 收到几个 HTTP 调用？为什么？

**F2.** worker 正在 PUT 时用户 unlink 了同一文件，最终 server 状态如何收敛？

**F3.** 为什么 daemon mount 的健康检查不能放在 fork 前？foreground 为什么可以？

**F4.** 往 writeback 挂载点 cp 一个 30MB 文件会发生什么？用户会看到错误吗？

**F5.** `ls` 看到 `.abstract` 但 `cat` 报 ENOENT，是什么机制在自愈？

---



## 维度 G：SQL / CLI

**G1.** `SELECT veda_write('/x','y')` 何时被拒？错误码是什么？两种不同的拒绝路径分别是什么？

**G2.** 为什么 `search('q')` 的向量查询发生在 SQL 计划期而不是执行期？有什么副作用？

**G3.** 一个跑在旧 server（0.1.13）上的新 CLI，执行 `veda cp photo.png` 会怎样？

---

---



#   答案要点（先自己答完再看）



## A

- **A1.** copy 时两 dentry 共享同一 file 记录，ref_count=2；重写 A 触发 COW——新建 file 给 A，旧 file ref_count 减回 1，B 不受影响。验证 `service/fs.rs:1657` finalize_full_rewrite 的 `ref_count>1` 分支。
- **A2.** 走 blob 路径。分流靠 `std::str::from_utf8` 嗅探 body，png 不是合法 UTF-8 → blob；Content-Type 只影响记录不影响分流。验证 `routes/fs.rs:96`。
- **A3.** 水印 `last_embedded_content_hash` 还等于旧内容 hash，worker 判「已 embed」跳过。正常写入路径会在 `update_file_revision` 里置 NULL，手工改 SQL 没走这步。修：置 NULL，或跑 `reconcile?dry_run=false`（force_reembed）。验证 `mysql.rs:1669/1262`。
- **A4.** 超 10 万上限，整个事务返回 QuotaExceeded 失败，全有全无不部分删。无分批 API，客户端得自己拆。验证 `service/fs.rs:24`。



## B

- **B1.** 写入不受影响（MySQL+outbox 同事务）；但 outbox 事件退避重试（5 次，间隔上限 1h）后进 dead，3 天足够全 dead。恢复靠 `POST /admin/v1/reconcile/{ws}?dry_run=false`，对账后 `force_reembed=true` 重新入队。验证 `mysql.rs:2105`、`reconciler.rs:267`。
- **B2.** status 停在 processing，lease 过期后被下次 claim 以 SKIP LOCKED 抢走，retry_count+1；若已到 max_retries，claim 时直接标 dead（不经 fail、无 backoff、无 error 记录）。已产生的 Milvus 副作用靠 upsert 幂等消化。验证 `mysql.rs:1977`。
- **B3.** embedding 可能因单块超 token 上限返回 400 而 dead，链式触发会让 L0/L1 和父目录聚合永远缺失。所以写入时两事件独立入队。验证 `service/fs.rs:1600` 附近注释。
- **B4.** 入队不去重（除非前一个还 pending），最多 10 个 ChunkSync 任务；但 worker 处理时靠水印匹配跳过冗余，实际 embedding 只 1-2 次（取决于处理顺序）。验证 `mysql.rs:1882`、水印检查 `worker.rs:289`。
- **B5.** ① embed → ② Milvus upsert → ③ delete_chunks_above 清旧尾块 → ④ 写水印。水印挪第一步：失败重试会被水印挡住，Milvus 永久缺数据。验证 `worker.rs:367`。
- **B6.** 缺陷：`complete()`/`fail()` 无条件按 id 改状态，无 `AND status='processing'`、无 fencing token，租约固定 10min 无续租。慢任务超租约被双 claim → 两 executor 读 v1/v2 内容交错写 Milvus + 各写水印 → Milvus 留混合 chunk 而水印谎称干净。地板修复：`complete`/`fail` 都加 `WHERE id=? AND status='processing'`（几行）。它只是地板因为解决不了「两 worker 都在 processing 状态下的交错写」——真解需 lease/fencing token 列 + 心跳续租。验证 `mysql.rs:1973/2035`、review A-3。



## C

- **C1.** mode 被静默忽略，走 semantic（embed query 查摘要集合），不报错。摘要集合无 BM25 索引，是实现限制。验证 `service/search.rs:52`。
- **C2.** ①prefix 过滤在 Rust 侧、Milvus 只 over-fetch 3x，候选里匹配前缀的不足；②目录摘要命中因 path 解析不到（dentry_id≠file_id）被前缀过滤直接丢。验证 `search.rs:123/167`。
- **C3.** 先看日志有没有 `hybrid_search_remote failed, falling back to ANN`。hybrid 失败静默降级为纯语义、只 warn，API 响应无任何指示。验证 `milvus.rs:1124-1127`。



## D

- **D1.** metrics_token 未配置、配成空串、或请求 token 错——三者都故意返回同样 404（不泄露端点存在性）。验证 `routes/mod.rs:67-117`。
- **D2.** 创建响应一次性返回；apps 面另存 `veda_workspace_keys.token` 明文列，可经 getToken（需过外部 authz）再取。native vk_ 面 mint 的 wk_ 只有 SHA-256 hash。验证 `apps.rs`、`mysql.rs:3098/3150`。
- **D3.** workspaces*→admin_token；tokens*→vk_（AuthAccount）；reconcile→metrics_token。三种互不相干。验证 `admin.rs` / `admin_tokens.rs` / `reconcile.rs`。
- **D4.** 数据面依然开着。wk_ 校验不查 workspace.status，靠 `delete_workspace` 事务级联 `UPDATE veda_workspace_keys SET status='revoked'`。手工 archive 绕过了级联 revoke。验证 `auth.rs:174` 注释、`mysql.rs:2908`。
- **D5.** `authorize()` 直接返回 `Ok(())`（fail-open），管理+数据面全部无外部鉴权，workspace_name 也解析成 null。**启动时无告警**（env 每请求现读，不在 ServerConfig）。验证 `platform.rs:112-133`（✅ 已核实注释原文）。



## E

- **E1.** socket activation 下 systemd 持端口，stop 之后任何排队连接会立刻用**磁盘上当前的 binary** 拉起服务。先 stop 后换 = 拉起旧版且后续 start no-op。验证 `scripts/deploy/deploy.sh:7-11`、runbook 铁律。
- **E2.** `migrate()` 每次启动跑幂等 ALTER/INDEX/backfill UPDATE，服务账号缺权限 fail-fast 起不来。验证 `mysql.rs:539` 附近、runbook 权限预检 SQL。
- **E3.** 跑 blob gate SQL（`storage_type='blob'` 计数 + 未消费 extract_sync 计数），任一 >0 只能 roll-forward。测试节点存在「显示 0.1.14 但含 blob 代码」的构建，认代码不认 version 号。验证 runbook 回滚节。
- **E4.** `/v1/abstract`、`/v1/overview` 返回 501（不是 202）；`/capabilities` 报 `summary_enabled:false`；FUSE 不显示 summary sidecar；worker 跳过摘要生成（且清既有摘要）。验证 `state.rs`、`search.rs`、`main.rs`。
- **E5.** 静默忽略这个值，保留 TOML 里的值（或默认 50）。`env_parse` 对格式错误的数字 env 不报错。验证 `config.rs:357`。



## F

- **F1.** 0 个。create() defer、写全进 shadow、tombstone LocalOnly 不发 DELETE、cancel+token 让堆条目自跳过。验证 commit_queue 测试 `vim_swap_full_lifecycle_produces_zero_server_calls`。
- **F2.** PUT 返回后 `is_current` 失败，检查 entry 已消失 + tombstoned + PUT 成功 → 补发 DELETE；若只是被新写覆盖（entry 还在）则绝不 DELETE。验证 `commit_queue.rs::fire()` + 测试 `stale_put_against_superseded_entry_does_not_delete`。
- **F3.** 父进程 pre-fork 碰 reqwest（内部 tokio 线程 + 连接池 fd 不过 fork）→ 子进程 SIGILL。foreground 单进程无 fork 所以 inline preflight 安全；daemon 把 preflight 放 child、错误经 pipe 非-'R' 帧回传。验证 `veda-fuse/src/main.rs:128` 附近注释。
- **F4.** 前 10MB 进 shadow；越界那次 write 返回 PerFileCapExceeded → sync_flush_and_evict_shadow（cancel+drain+同步 PUT+踢出 shadow）→ 余下走 legacy buf 同步 PUT。用户全程无感（EFBIG 只在 setattr-truncate 超 cap 时出现）。验证 `fs.rs` write() + sync_flush_and_evict_shadow。
- **F5.** readdir 注入靠 mount 时 capabilities 探测（fail-open）；`cat` 触发 lookup→GET summary 404/501→写 per-dir miss cache→下轮 readdir 不再注入，TTL（≥1s）后重试；summary_ready SSE 事件会定点清 miss cache。验证 `fs.rs` magic_lookup/note_sidecar_missing、`sse.rs`。



## G

- **G1.** ①read-only wk_ → typed `PermissionDenied` → 403（靠 DataFusionError::External + find_root 恢复，不塌成 500）；②DDL/DML/statements → 任何 key 都被 `SQLOptions` 在 planner 层拒。平台网关 SQL 恒 read_only。验证 `engine.rs:132`、sql_test `read_only_rejects_write_udf`。
- **G2.** UDTF 的 `call()` 是同步的、在 planning 阶段执行 embedding+Milvus 查询并整表物化进 MemTable。副作用：查询计划期就发外部请求，无法下推 limit 到向量层之外的过滤，大结果集吃内存。验证 `search_table.rs` TableFunctionImpl::call。
- **G3.** 返回 400。binary cp 需要 blob 支持的 server（≥0.1.15）；CLI↔server 无版本握手，旧 server 不认 blob 存储直接拒。验证 CHANGELOG Unreleased、`routes/fs.rs` blob 分支。

