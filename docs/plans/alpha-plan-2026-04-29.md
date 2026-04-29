# Veda Alpha 6 周执行计划

> **状态**：Draft（已和 Joe 对齐范围与节奏）
> **目标**：4 月 29 日起 6 周内完成 alpha 试用版，给工程师朋友试用 FUSE 形态
> **约束**：一人推进 / 单副本部署 / 虚拟机 / 还无真实用户 / b 形态（自托管 + 内部多团队的 baseline，但 alpha 期单副本足够）
> **后续多副本演进**：用 Zookeeper 做集群成员管理 + per-file_id 分桶（不在本计划范围）

---

## 1. 范围边界

### 必须做（alpha-blocker）

- **A. 数据正确性**：Worker 原子化（content_hash 跳过）+ ChunkSync 入队去重 + Reconciler。三件套缺一漏一类风险
- **B. 大文件链路**：`read_file_range` chunked-aware + worker 流式 embed + FUSE read 溢出防护
- **C. 多人协作可信**：fs_events retention + cursor 410 失效协议 + path_prefix server-side 过滤
- **D. FUSE 工程师必踩三件套**：mtime 用真实时间、SSE delete 清 inode、SSE timeout(120s)
- **E. 可观测性最低限**：Prometheus metrics（outbox/embed/pool/drift/http）
- **F. 部署解耦最小集**：MySQL pool 五参数化 + migration 拆 binary（worker 暂不拆）
- **G. 单机部署模板 + alpha 文档**

### 明确不做（写入 Known Limitations）

| 项 | 理由 | 提前到 beta 的触发条件 |
|---|---|---|
| 同 file_id 任务串行化 | 单副本 + 6h reconciler 兜底 | 多副本部署前必须做 |
| Worker 拆独立 binary | 单副本无意义 | 多副本部署前必须做 |
| Doc-level ACL / Quota | 内部团队信任，无外部 partner | 对外 SaaS 之前 |
| Login 防爆 / JWT kid 轮换 | 内部网络 | 公网暴露之前 |
| FUSE setattr 截断 / WriteHandle spill / CLI Cp 流式 | 50MB 上限够用，标 known limitation | 用户开始大文件抱怨时 |
| SSE 心跳 / long-lag full resync | 单副本 + retention=7d 够用 | 出现 cursor 落后 retention 窗口的报告时 |
| SQL SessionContext 缓存 | <50 collection 不痛 | 发现单 SQL 请求 >2s 时 |
| Tree-sitter chunking / PDF/OCR / Citation 稳定 | 试用反馈驱动 | 试用者抱怨内容类型或引用断时 |
| 真 hybrid search (BM25) / Milvus partition | 数据量小召回还能用 | 单 workspace > 100k chunks 时 |
| OpenAPI / SDK / Connector | partner 需求驱动 | 第一个外部 partner 接入前 |

---

## 2. 集成测试基础设施

### 2.1 复用现有约定

- 路径：`config/test.toml`（已 gitignored）
- 模板：`config/test.toml.example`
- 标记：`#[ignore]` + `cargo test -- --ignored`
- 加载：`PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/test.toml")`

### 2.2 计划期间扩展 test.toml.example

需要新增 `[llm]` 段（用于 LLM retry / summary 集成测试）。最终模板：

```toml
[mysql]
database_url = "mysql://user:password@localhost:3306/veda"

[milvus]
url = "http://localhost:19530"
token = ""
db = "default"

[embedding]
api_url = "https://api.openai.com/v1/embeddings"
api_key = "sk-xxx"
model = "text-embedding-3-small"
dimension = 1024

# 新增：LLM provider for summary/L0/L1
[llm]
api_url = "https://api.openai.com/v1/chat/completions"
api_key = "sk-xxx"
model = "gpt-4o-mini"
max_summary_tokens = 2048
```

**Week 1 第一天先扩 example**，之后按周加用例。

### 2.3 集成测试组织约定

- 单测（`#[test]` 或 `#[tokio::test]`）：纯逻辑、不连外部
- 集成测试（`#[tokio::test]` + `#[ignore]`）：必须连真实 MySQL + Milvus + LLM/Embedding
- 每个集成测试应该幂等可重跑：开头清理、结尾清理（用唯一 workspace_id）

---

## 3. 6 周路线图

每周一个主题串行做。Week 6 是显式 buffer 周。

| 周 | 主题 | 关键产出 |
|---|---|---|
| 1 | 数据正确性闭环 | content_hash 跳过 + outbox 去重 |
| 2 | Reconciler | drift 检测与自愈 binary |
| 3 | Metrics + schema 解耦 | /metrics + veda-migrate |
| 4 | 大文件链路 + FUSE 三件套 | OOM 防护 + mtime/delete/timeout 修复 |
| 5 | 协作可信 + alpha 收尾 | retention/410/path_prefix + docker-compose + 中文 alpha 文档 |
| 6 | Buffer + 自己 dogfood | 不安排新工作 |

---

## Week 1 · 数据正确性闭环

### 任务清单

| # | 任务 | 文件 | 估时 |
|---|---|---|---|
| 1.1 | `veda_files` schema 加 `last_embedded_content_hash VARCHAR(64) NULL` 字段 | `crates/veda-store/src/mysql.rs` (migrate fn) | 0.5d |
| 1.2 | `MetadataTx` trait 加 `update_file_content_hash(file_id, hash)` 方法 + Mysql 实现 | `crates/veda-core/src/store.rs`, `crates/veda-store/src/mysql.rs` | 0.5d |
| 1.3 | `worker.handle_chunk_sync` 开头比对 hash：相同则跳过 embed/upsert，直接 complete | `crates/veda-server/src/worker.rs` | 0.5d |
| 1.4 | embed 完成后写回 `last_embedded_content_hash = file.checksum_sha256` | 同上 | 0.5d |
| 1.5 | 写路径入队 `OutboxEventType::ChunkSync` 前调 `has_pending_event` 去重 | `crates/veda-core/src/service/fs.rs` (make_outbox 调用点) | 0.5d |
| 1.6 | 审 `enqueue_summary_sync` (worker.rs:96)：能合到 ChunkSync 事务的就合 | `crates/veda-server/src/worker.rs` | 0.5d |
| 1.7 | test.toml.example 加 `[llm]` 段，write 集成测试用 | `config/test.toml.example` | 5min |

### 单测

- `mysql.rs`：`update_file_content_hash` 写入与读出
- `worker.rs`（mock store）：相同 hash 时不调 vector.upsert / embedding.embed
- `worker.rs`：不同 hash 时正常走完流程并写回新 hash
- `fs.rs`：连续 5 次 write 同一文件 → outbox 表只有 1 条 ChunkSync (status=pending)

### 集成测试（新增）

文件：`crates/veda-server/tests/worker_atomic_test.rs`（新文件）

```rust
#[tokio::test]
#[ignore]
async fn worker_skips_embed_on_unchanged_content_hash() {
    // 1. 加载 test.toml，连真实 MySQL + Milvus + Embedding
    // 2. 创建 workspace_id_unique
    // 3. write_file(content_a) → 等 worker 处理完
    // 4. 记录 milvus chunk count 和 embedding API 调用次数（用 wrap embedding service 计数）
    // 5. write_file(content_a) 再写一遍同样内容（content_unchanged=true 路径）
    // 6. 等 worker poll 一轮（5s）
    // 7. 断言 embedding 调用次数没变（只调用了 1 次）
    // 8. cleanup: delete workspace
}

#[tokio::test]
#[ignore]
async fn worker_dedupe_pending_chunksync() {
    // 1. write 同一文件 5 次（content 每次不同，避免 content_unchanged 短路）
    // 2. 立刻查 veda_outbox 表
    // 3. 断言 status=pending 的 ChunkSync 只有 1 条（后续都被 has_pending_event 短路）
}
```

### 验收

- `cargo test --workspace`（148 + 新增）全过
- `cargo test --workspace -- --ignored` 全过（22 + 新增 2）
- 启动 server，手动 write 同一文件 5 次 → 看 worker 日志只 embed 1 次

### 风险

- `has_pending_event` 当前用 `JSON_EXTRACT` 全表过滤（§3.7 指出过），频繁写入时会慢。如果开发期发现明显慢，先在 `veda_outbox` 加 `payload_file_id VARCHAR(64) AS (JSON_UNQUOTE(JSON_EXTRACT(payload, '$.file_id'))) STORED` + index——但这是优化，正确性不受影响

---

## Week 2 · Reconciler

### 任务清单

| # | 任务 | 文件 | 估时 |
|---|---|---|---|
| 2.1 | 在 worker 进程内加 cron 模块（默认 6h，可 config） | `crates/veda-server/src/worker.rs` 或新文件 `reconciler.rs` | 0.5d |
| 2.2 | 算法：拉 MySQL `veda_files.id` set vs Milvus `veda_chunks` 中 file_id distinct set | 新 trait method `VectorStore::list_distinct_file_ids(workspace_id)` 或直接 query API | 1.5d |
| 2.3 | 双向 diff：MySQL 有/Milvus 无 → enqueue ChunkSync；Milvus 有/MySQL 无 → 直接 delete | `reconciler.rs` | 1d |
| 2.4 | `veda_summaries` 同样对账逻辑 | 同上 | 0.5d |
| 2.5 | metric `veda_drift_total{type=missing\|orphan}` 暴露 | 同上（依赖 Week 3 的 metrics 框架；本周先 tracing log，Week 3 替换为 metric） | 0.5d |
| 2.6 | ServerConfig 加 `[reconciler]` 段（enabled, interval_secs, batch_size） | `crates/veda-server/src/config.rs` | 0.5d |

### 单测

- 算法层（mock store）：制造 3 种漂移（MySQL 多 / Milvus 多 / chunk_index 越界）→ 验证 diff 输出正确
- diff 应用层：对 missing 调 enqueue ChunkSync；对 orphan 调 delete_chunks

### 集成测试（新增）

文件：`crates/veda-server/tests/reconciler_test.rs`

```rust
#[tokio::test]
#[ignore]
async fn reconciler_clears_orphan_milvus_chunks() {
    // 1. 真 MySQL+Milvus 起一个 workspace
    // 2. write_file(A) → 等 worker 完成 → Milvus 有 A 的 chunks
    // 3. 直接 SQL DELETE FROM veda_files WHERE id=A_file_id（绕过 service）
    //    → 制造 "Milvus 有 / MySQL 无" 的漂移
    // 4. 跑 reconciler.run_once()
    // 5. 断言 Milvus 中 file_id=A 的 chunks 已清零
}

#[tokio::test]
#[ignore]
async fn reconciler_reembeds_missing_chunks() {
    // 1. write_file(A) → Milvus 有 chunks
    // 2. 直接调 milvus.delete_chunks(A) → 制造 "MySQL 有 / Milvus 无"
    // 3. 跑 reconciler.run_once()
    // 4. 等 outbox worker 处理完新 enqueue 的 ChunkSync
    // 5. 断言 Milvus 中 A 的 chunks 数量恢复
}

#[tokio::test]
#[ignore]
async fn reconciler_handles_summary_drift() {
    // 类似上面但针对 veda_summaries
}
```

### 验收

- 三个集成测试全过
- 在干净 workspace 跑一次 reconciler，0 漂移
- 故意制造漂移 → 跑 → 漂移清零

### 风险

- Milvus 没有现成的 "list distinct file_id" API。可能需要 `entities/query` + 客户端去重，对大数据集慢。alpha 期可接受 O(N) 拉取，beta 时考虑 partition 或单独 indexing
- reconciler 与 worker 在同一进程跑，可能争 MySQL pool。Week 3 pool 参数化时给 reconciler 单独的 connection budget

---

## Week 3 · Metrics + Schema 解耦

### 任务清单

| # | 任务 | 文件 | 估时 |
|---|---|---|---|
| 3.1 | 接入 `axum-prometheus` crate | `crates/veda-server/Cargo.toml`, `main.rs` | 0.5d |
| 3.2 | 暴露 `/metrics` 端点 + 7 类 metric | `crates/veda-server/src/routes/mod.rs` 或新 `metrics.rs` | 1d |
| 3.3 | metric 列表实现（详见下表） | 多文件埋点 | 1d |
| 3.4 | MySQL pool 五参数化（min/max/idle/acquire/lifetime + ServerConfig + env override） | `crates/veda-store/src/mysql.rs`, `crates/veda-server/src/config.rs` | 0.5d |
| 3.5 | 拆 `veda-migrate` binary | 新 crate `crates/veda-migrate` 或 `crates/veda-server/src/bin/migrate.rs` | 1d |
| 3.6 | server 启动加 `--skip-migrate` flag（默认 true，在 server 模式跳过 migrate） | `crates/veda-server/src/main.rs` | 0.5d |
| 3.7 | docker-compose 加 prometheus 服务 + grafana（可选） + 一个 dashboard JSON 模板 | `deploy/docker-compose.yml`, `deploy/grafana/` | 0.5d（Week 5 复用） |

### 必备 metric 清单

| 名称 | 类型 | label | 来源 |
|---|---|---|---|
| `veda_outbox_depth` | gauge | status, event_type | mysql.rs 周期 select count(*) |
| `veda_outbox_failed_total` | counter | event_type | task_queue.fail() |
| `veda_outbox_dead_total` | counter | event_type | task_queue.fail() 转 dead 时 |
| `veda_embed_latency_seconds` | histogram | provider | embedding.rs |
| `veda_llm_latency_seconds` | histogram | provider | llm.rs |
| `veda_mysql_pool_connections` | gauge | - | sqlx pool.size() |
| `veda_mysql_pool_idle` | gauge | - | sqlx pool.num_idle() |
| `veda_drift_total` | gauge | type=missing\|orphan | reconciler |
| `veda_http_request_duration_seconds` | histogram | route, method, status | axum middleware |

### 单测

- pool config 反序列化测试（已有 config_test 模式）
- migrate binary 启动时不 panic（可用 `assert_cmd` 跑 binary）

### 集成测试（新增）

文件：`crates/veda-server/tests/metrics_test.rs`

```rust
#[tokio::test]
#[ignore]
async fn metrics_endpoint_exposes_outbox_depth() {
    // 1. 起 server with real mysql+milvus
    // 2. write 几个 file 制造 pending outbox events
    // 3. GET /metrics
    // 4. 断言 body 包含 "veda_outbox_depth{status=\"pending\"" 且数值 > 0
}

#[tokio::test]
#[ignore]
async fn metrics_endpoint_exposes_pool_status() {
    // GET /metrics → 检查 veda_mysql_pool_connections / veda_mysql_pool_idle 存在
}

#[tokio::test]
#[ignore]
async fn migrate_binary_creates_schema_idempotently() {
    // 1. 起一个空 mysql database
    // 2. 跑 veda-migrate config.toml 一次
    // 3. 跑 veda-migrate config.toml 第二次（应当 noop / 不报错）
    // 4. 断言 schema 各表存在
}
```

### 验收

- `curl localhost:3000/metrics` 返回 prometheus text
- 在 docker-compose 起 prometheus → 能 scrape 成功 → 看到所有 metric
- veda-migrate 独立 binary 能跑且幂等

### 风险

- axum-prometheus 与现有 TraceLayer 顺序敏感，注册顺序错可能导致部分指标缺失。验证时 grep `veda_http_request_duration_seconds` 必须存在
- migrate binary 与 server 共享 store crate：Cargo workspace dep 设置好，避免重新编译整个依赖

---

## Week 4 · 大文件链路 + FUSE 三件套

### 任务清单

| # | 任务 | 文件 | 估时 |
|---|---|---|---|
| 4.1 | `read_file_range` chunked-aware（按 byte 偏移定位 chunks，只读必需） | `crates/veda-core/src/service/fs.rs` | 1d |
| 4.2 | 顺手修 §2.2 SQL 边界 bug：外层 `WHERE start_line + line_count > ?` | `crates/veda-store/src/mysql.rs` (get_file_chunks_conn) | 0.5d |
| 4.3 | worker `handle_chunk_sync` 流式：用 chunk-by-chunk 迭代器替换全文 String 拉取 | `crates/veda-server/src/worker.rs` | 1d |
| 4.4 | FUSE `read` 路径加 `saturating_add` 防溢出（§8.5） | `crates/veda-fuse/src/fs.rs` | 5min |
| 4.5 | **FUSE mtime**：`api::FileInfo` 添加 `created_at`/`updated_at` 反序列化（client 端）+ `make_attr` 用真实时间 | `crates/veda-fuse/src/client.rs`, `fs.rs` | 0.5d |
| 4.6 | **FUSE mtime**：`DirEntry` DTO 也加 timestamps（如果 server `list_dir` 返回；否则 dir cache 退化用 server-known 时间） | 同上 | 0.5d |
| 4.7 | **FUSE SSE delete**：`invalidate_caches` 区分 delete/move，delete 走 `inode_remove` | `crates/veda-fuse/src/sse.rs` | 0.5d |
| 4.8 | **FUSE SSE timeout**：`reqwest::Client::builder().timeout(Some(Duration::from_secs(120)))` + 主动 reconnect | 同上 | 0.5d |
| 4.9 | Dogfood：本地 mount FUSE，跑 `make`、`vim` 编辑、`rsync -av` | — | 1d |

### 单测

- `read_file_range` chunked path：mock store + chunks `[(0, 1000), (1000, 2000), (2000, 3000)]`，请求 byte range `1500-2500` → 应只读 chunk 1 和 2（不应读 chunk 0）
- §2.2 SQL 边界单测（用 mysql_test 集成测试形式覆盖）
- worker 流式 embed：mock 返回 chunks 迭代器，验证不分配 size_total 大小的 String
- FUSE `read` panic test：传入 offset=0, size=u32::MAX → 不 panic（saturating_add）
- FUSE mtime：mock client 返回 `updated_at = 2026-04-01T00:00:00Z` → make_attr 输出 SystemTime 对应该时间
- FUSE SSE delete：模拟 `event_type="delete"` 事件 → 验证 `inode_remove` 被调

### 集成测试（新增）

文件：`crates/veda-server/tests/large_file_streaming_test.rs`

```rust
#[tokio::test]
#[ignore]
async fn read_file_range_does_not_load_full_chunked_file() {
    // 1. write 一个 49MB 的 chunked 文件（够大但在限制内）
    // 2. 多次 GET /v1/fs/path with Range: bytes=24M-25M
    // 3. 通过 metrics（mysql_pool_connections + mysql 查询数）间接验证只读了相关 chunks
    //    或者：在 fs_service 内部 mock 计数器（用 tracing event 计数）
}

#[tokio::test]
#[ignore]
async fn worker_streams_chunks_for_large_file_embed() {
    // 1. write 49MB chunked 文件
    // 2. 跑 worker 处理 ChunkSync
    // 3. 通过 metric 或 RSS 间接验证 worker 内存峰值合理（< 100MB 总）
    //    （这条比较难严格验证，先用 tracing log 输出 chunk 数量验证流式）
}

#[tokio::test]
#[ignore]
async fn get_file_chunks_returns_empty_when_range_exceeds_eof() {
    // 1. write 100 行的 chunked 文件
    // 2. 直接调用 store.get_file_chunks(file_id, start_line=200, end_line=300)
    // 3. 断言返回空 Vec（不再返回 last chunk）
}
```

文件：`crates/veda-fuse/tests/integration_test.rs`（如果当前没有）

FUSE 集成测试比较复杂（需要真 mount），暂用单测覆盖。如果 Joe 愿意，Week 6 可以补一个用 `tempdir + fuser::spawn_mount` 的烟雾测试。

### 验收

- 49MB 文件按 byte range 读 10 次，server RSS 增长 < 5MB
- 本地 FUSE 挂载，`make -B` 在 mounted dir 内能跑通（mtime 不再乱跳）
- 删除 FUSE 内文件后 `stat` 立刻返 ENOENT
- SSE 连接断开后 120s 内自动重连

### 风险

- 流式 chunk 迭代要避开 `tx.get_file_chunks` 一次性返回 Vec 的现状，需要新加 trait method 或者 stream API。如果改造成本高，**可降级方案**：分页拉取（每次 10 chunks），保留单次 query 但分页。流式不是绝对必要，**alpha 限制 50MB → 一次性拉也只是 ~200 chunks，~50MB**，先用分页 + buffer 复用即可
- FUSE mtime 修改影响 readdirplus 路径：要保证 dir cache 也更新，否则 `ls -l` 看到旧时间

---

## Week 5 · 协作可信度 + alpha 收尾

### 任务清单

| # | 任务 | 文件 | 估时 |
|---|---|---|---|
| 5.1 | `veda_fs_events` retention：每天 cron 删 N 天前事件 | `crates/veda-server/src/worker.rs`（同 reconciler 一起放） + config | 0.5d |
| 5.2 | `/v1/events` cursor 校验：if `since_id < MIN(id)` → return HTTP 410 + body `{current_min_id: N}` | `crates/veda-server/src/routes/events.rs` | 0.5d |
| 5.3 | `/v1/events?path_prefix=` server-side 过滤（已有 `query_fs_events` 接口支持，串通到 SSE）| 同上 | 0.5d |
| 5.4 | FUSE 收到 410 → `invalidate_all` + 重新 readdir + cursor 重置到 `current_min_id` | `crates/veda-fuse/src/sse.rs` | 0.5d |
| 5.5 | docker-compose 单机一键起整套（mysql + milvus + server + 内置 worker + prometheus + grafana） | `deploy/docker-compose.yml` | 1d |
| 5.6 | systemd unit 模板（Joe 可能更喜欢直接 systemd 而非 docker） | `deploy/systemd/veda-server.service`, `veda-migrate.service` | 0.5d |
| 5.7 | 写 `docs/alpha-tryout.md`（中文）：5 步上手 + Known Limitations + 反馈渠道 | 文档 | 1d |
| 5.8 | 三客户端模拟测试：A 在线 / B 离线 8 天 / C 持续 → B 收到 410 后视图正确 | 集成测试 | 1d |

### 单测

- 410 响应格式正确
- path_prefix 过滤逻辑（已有 `query_fs_events` 接口加单测）
- FUSE `handle_410` 函数：mock 状态 → 验证 cache 清空 + cursor 重置

### 集成测试（新增）

文件：`crates/veda-server/tests/events_retention_test.rs`

```rust
#[tokio::test]
#[ignore]
async fn cursor_below_min_id_returns_410() {
    // 1. write 几次产生 events
    // 2. 直接 SQL DELETE FROM veda_fs_events WHERE id < some_id（模拟 retention）
    // 3. GET /v1/events?since_id=0
    // 4. 断言 status=410 + body 含 current_min_id
}

#[tokio::test]
#[ignore]
async fn events_path_prefix_filter() {
    // 1. write /docs/a.md, /src/b.rs, /docs/c.md
    // 2. GET /v1/events?path_prefix=/docs
    // 3. 断言只返回 /docs/* 的 2 条事件
}

#[tokio::test]
#[ignore]
async fn three_client_collaboration_simulation() {
    // Client A: 持续 SSE 订阅，连续 write 5 个文件
    // Client B: 订阅后断开，等 10s（模拟离线），再连 with stale cursor
    // Client C: 持续订阅
    // 验证 B 收到 410 → 重新 list_dir 看到正确的最终状态
    // C 持续接收所有事件
}

#[tokio::test]
#[ignore]
async fn fs_events_retention_cleans_old_rows() {
    // 1. 直接插入 50 个事件，时间戳分布在 1-30 天前
    // 2. 跑 retention task（手动触发，retention_days=7）
    // 3. 查 veda_fs_events，断言只剩 7 天以内的
}
```

### 验收

- docker-compose 一条 `docker compose up` 起整套，30s 内全 healthy
- alpha 文档自己照走一遍 30 分钟内能起起来
- 三客户端模拟测试通过
- 干净虚拟机上跟着 alpha 文档能起起来（自己拿一台测试虚拟机走一遍）

### 风险

- docker-compose 里 milvus 启动慢（30-60s），需要 `depends_on: condition: service_healthy`
- systemd unit 写法 + 数据目录权限要小心，确保 `veda` 用户能读 config

---

## Week 6 · Buffer + Dogfood

### 用法

不安排新任务。两种用法之一：

**用法 A：前面溢出**
- Week 1 的 has_pending_event 性能问题 → 加 generated column index
- Week 2 的 reconciler API 难度 → 改成直接 milvus query 客户端去重
- Week 4 的流式实现复杂度 → 退化为分页方案
- Week 5 的 docker-compose 编排细节

**用法 B：自己重度 dogfood**
- 起 5 个 workspace（不同主题：代码 / 文档 / 笔记 / 日志 / 数据）
- 每个 workspace 写 100+ 文件（用 git clone + cp -r 模拟）
- 跑 SQL（`SELECT veda_fs('/'')`、search、count 等）
- FUSE 挂载到本地 dev 环境，实际写 Rust 代码、跑 cargo build
- 看 prometheus 指标 24 小时趋势
- 故意制造异常（kill server、断网、撑爆 50MB 限制等）→ 看反应

**目标**：让第一波试用者别踩到我自己一周内能踩到的坑。

---

## 4. Known Limitations 清单（写入 alpha-tryout.md）

```markdown
## 已知限制（Alpha 阶段）

### 性能与规模
- 单文件大小硬上限 50MB
- 通过 FUSE 写入 >10MB 文件可能较慢
- 当前为单副本部署，多副本支持需要等下一阶段（计划用 Zookeeper 做集群协调）

### 权限与安全
- 仅 workspace 级权限，所有成员可读可写所有文件
- 无登录失败次数限制 / 无 IP 频控 / 无 token 主动撤销，JWT 24h 自动过期
- 推荐部署在内网

### FUSE 客户端
- 改 chmod / chown 静默忽略
- 文件稀疏写（如 seek 到 1GB 写 1 字节）会把空洞按零字节加载到内存

### CLI
- `veda cp` 把文件全量读入内存，不要用于 >10MB 文件，用 HTTP API 直接 PUT

### 搜索
- 全文搜索基于 LIKE 模式匹配，不是 BM25
- 搜索结果引用 chunk_index 不稳定，文件重写后历史引用可能错位

### 文档类型
- 仅支持 text/plain
- PDF / 图片 OCR 未实现

### 部署
- Worker 内嵌于 server 进程，多副本部署不支持
- 推荐 docker-compose 或单机 systemd

### 数据正确性
- 同一文件写入与删除若在 worker 同批次内并发处理（概率低），可能短期向 Milvus 留下孤儿向量
- Reconciler 每 6 小时清理一次（可配置）
- 用户感知层面无影响（孤儿 chunk 在搜索时已被路径解析过滤）
```

---

## 5. 节奏 / 风险 / 决策点

### 决策点（每周末检查）

- **Week 1 末**：去重逻辑必须可见效（手测 + ut + it 三层验证）。如果 has_pending_event 性能崩，立刻 Week 2 给 outbox 加 generated column
- **Week 2 末**：Reconciler 必须能在 alpha 部署前跑出 0 漂移
- **Week 3 末**：docker-compose 必须能起 prometheus + 看到至少 outbox_depth metric
- **Week 4 末**：本地 FUSE mount 必须能 `make` / `rsync` 通
- **Week 5 末**：自己照文档在干净虚拟机起一次，30 分钟阈值
- **Week 6 末**：决定是否真的开放给 alpha 试用者

### 不可让步的两条底线

1. Reconciler 在 alpha 第一次部署前必须**手动跑过一次**清零漂移
2. Metrics 至少有 `outbox_depth + outbox_failed + outbox_dead` 三项可见

### 灵活让步的（按需降级）

- Week 4 流式 → 分页（200 chunks 一次性 50MB 也能接受）
- Week 5 grafana dashboard → prometheus text 看也行
- Week 5 systemd → 只给 docker-compose

---

## 6. 后续路线（不在本计划）

按 alpha 试用反馈优先级排序：

1. **多副本支持**：Zookeeper 集群成员管理 + per-file_id 分桶 + worker 拆 binary
2. **Doc-level ACL + Quota**（依赖 P1-6 account_id 修复）
3. **大文件 FUSE 完整支持**（setattr 截断 server-side API、WriteHandle spill to disk）
4. **多文档类型**：PDF / OCR / tree-sitter chunking
5. **真 hybrid search**（BM25 或 Milvus 2.5+ FTS）
6. **审计日志 + OpenTelemetry**
7. **Connector / OpenAPI / SDK**

---

## 附录：每周 cargo test 命令

```bash
# 单测（每周必跑）
cargo test --workspace

# 集成测试（每周新增的必跑，需要 config/test.toml）
cargo test --workspace -- --ignored

# 只跑某个 crate 的集成测试
cargo test -p veda-server -- --ignored

# 只跑某个测试
cargo test --workspace --test reconciler_test -- --ignored
```

每次 commit 前必跑：`cargo test --workspace && cargo test --workspace -- --ignored`。
