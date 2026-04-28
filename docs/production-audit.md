# Veda Production-Readiness Audit

> 自托管 + 内部 AI 功能后端 + 10 个 builder 场景下的 production gap 分析。
> 审计日期：2026-04-28
> 审计版本：v1.0

---

## 目录

- [P0 - 必须修复（真的会出事）](#p0---必须修复真的会出事)
- [P1 - 平台姿态缺陷（多租户站不住）](#p1---平台姿态缺陷多租户站不住)
- [P2 - 规模化/自助化前的债务](#p2---规模化自助化前的债务)
- [修复优先级建议](#修复优先级建议)

---

## P0 - 必须修复（真的会出事）

### P0-1. 没有 Reconciler，MySQL ↔ Milvus 漂移无法自愈

**严重程度**: 🔴 Critical

**位置**: `ARCHITECTURE.md` 已标记为 TODO

**问题描述**:
- Outbox 模式假设"事件不丢 + worker 不丢"
- 任何一次事件丢失或处理错误，MySQL 和 Milvus 永久不一致
- 具体不一致场景：
  - Worker crash 后 lease 超时，任务被重新 claim → 重复 embed → API 费用浪费
  - MySQL 写入成功但 outbox 写入失败 → 数据只有 MySQL 有
  - Milvus 写入成功但 MySQL complete() 失败 → 任务被重新执行

**代码证据**:
```rust
// worker.rs:94-95
self.handle_chunk_sync(&task.workspace_id, file_id).await?;
self.task_queue.complete(task.id).await?;
// 中间 crash → lease 超时 → 重新 claim → 再 embed 一次
```

**修复方案**:
1. 实现 Reconciler：周期性对比 MySQL 和 Milvus 数据
2. 以 MySQL 为 source of truth，修复 Milvus 缺失/多余数据
3. 运行频率可配置（默认每 6 小时）

---

### P0-2. Worker 处理与 complete() 非原子

**严重程度**: 🔴 Critical

**位置**: `worker.rs:94-95`, `mysql.rs:1574-1636`

**问题描述**:
- `process_task` 成功 → `complete()` 是两步操作
- 中间 crash 导致：
  1. lease 超时（10分钟）
  2. 任务被重新 claim
  3. 再次调用 embedding API（重复付费）

- `handle_chunk_sync` 内部 `delete_chunks` + `upsert_chunks` 也不是原子：
  - crash 在中间 → partial chunks 残留

**代码证据**:
```rust
// worker.rs:94-95
self.handle_chunk_sync(&task.workspace_id, file_id).await?;
self.task_queue.complete(task.id).await?;  // ← crash here

// milvus.rs:488-527 - 两步操作
self.post("/v2/vectordb/entities/upsert", body).await?;
self.post("/v2/vectordb/entities/delete", del).await?;  // ← crash here
```

**修复方案**:
1. 在 MySQL 记录 `last_embedded_content_hash`，worker 开始时检查
2. 如果 hash 未变，跳过 embed（幂等）
3. Milvus 操作改为：先 upsert 新数据，再 delete stale（当前顺序）
4. 考虑使用 MySQL 事务包裹 outbox 状态变更

---

### P0-3. `/v1/health` 永远返回 ok

**严重程度**: 🔴 Critical

**位置**: `routes/mod.rs:26-28`

**问题描述**:
- 不检查 MySQL / Milvus / Embedding 服务可用性
- K8s liveness probe 永远通过
- 后端服务挂掉时，LB 仍会把流量打向死实例

**代码证据**:
```rust
// routes/mod.rs:26-28
async fn health() -> Json<ApiResponse<&'static str>> {
    Json(ApiResponse::ok("ok"))
}
```

**修复方案**:
1. 实现 `/v1/health` 检查 MySQL 连接池
2. 实现 `/v1/ready` 检查 Milvus + Embedding 服务
3. 返回各组件状态，便于排查

```rust
async fn health(State(state): State<Arc<AppState>>) -> Json<HealthResponse> {
    let mysql_ok = state.meta.ping().await.is_ok();
    let milvus_ok = state.vector.ping().await.is_ok();
    Json(HealthResponse {
        status: if mysql_ok && milvus_ok { "healthy" } else { "degraded" },
        components: vec![
            ("mysql", mysql_ok),
            ("milvus", milvus_ok),
        ],
    })
}
```

---

### P0-4. embed() 不切批、不重试

**严重程度**: 🔴 Critical

**位置**: `worker.rs:178`, `pipeline/embedding.rs:89`

**问题描述**:
- 大文件 chunk 数可能超过 OpenAI batch 限制（2048 chunks/request）
- 无 exponential backoff
- 无 429 rate limit 处理
- 单次失败导致整个 task 失败

**代码证据**:
```rust
// worker.rs:177-178
let texts: Vec<String> = sem_chunks.iter().map(|c| c.content.clone()).collect();
let embeddings = self.embedding.embed(&texts).await?;  // ← 全部塞进去
```

**修复方案**:
1. 切批：每批 max 100 chunks（留安全余量）
2. 实现 exponential backoff + jitter
3. 429 响应时解析 `Retry-After` header
4. 记录失败批次，支持部分重试

```rust
for batch in texts.chunks(100) {
    let embeddings = retry_with_backoff(|| self.embedding.embed(batch)).await?;
    // ...
}
```

---

### P0-5. Secret 全在明文 TOML，环境变量覆盖未实现

**严重程度**: 🔴 Critical

**位置**: `config.rs:89-92`

**问题描述**:
- `jwt_secret`、`embedding.api_key`、`llm.api_key`、`mysql.database_url` 全在 TOML 文件
- README 声称支持 `VEDA_` 环境变量覆盖，但代码未实现
- 生产环境必须支持：
  - 环境变量
  - Kubernetes Secret 挂载
  - Vault 集成（可选）

**代码证据**:
```rust
// config.rs:89-92
impl ServerConfig {
    pub fn load(path: &str) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(path)?;
        Ok(toml::from_str(&raw)?)  // ← 只读 TOML，没有 env override
    }
}
```

**修复方案**:
```rust
impl ServerConfig {
    pub fn load(path: &str) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(path)?;
        let mut cfg: ServerConfig = toml::from_str(&raw)?;

        // Env overrides
        if let Ok(v) = std::env::var("VEDA_JWT_SECRET") {
            cfg.jwt_secret = v;
        }
        if let Ok(v) = std::env::var("VEDA_MYSQL_URL") {
            cfg.mysql.database_url = v;
        }
        if let Ok(v) = std::env::var("VEDA_EMBEDDING_API_KEY") {
            cfg.embedding.api_key = v;
        }
        // ... 其他字段

        Ok(cfg)
    }
}
```

---

### P0-6. 二进制文件写入会 panic

**严重程度**: 🔴 Critical

**位置**: `veda-core/src/service/fs.rs:134`

**问题描述**:
- 文件写入强制 UTF-8 验证：`std::str::from_utf8(slice).expect(...)`
- 二进制文件（图片、PDF、压缩包）写入会导致 panic
- API 层已做 UTF-8 验证（`routes/fs.rs:79`），但更底层不应 panic

**代码证据**:
```rust
// fs.rs:134-135
let chunk_content = std::str::from_utf8(slice)
    .expect("chunk boundary must align with UTF-8")  // ← panic!
    .to_string();
```

**修复方案**:
1. 存储 `Vec<u8>` 而非 `String`
2. 或在 API 层明确拒绝二进制文件，返回 415 Unsupported Media Type
3. 添加 `mime_type` 检查，只允许 `text/*` 和 `application/json`

---

## P1 - 平台姿态缺陷（多租户站不住）

### P1-1. 只有 workspace 级隔离，没有 row-level / doc-level ACL

**严重程度**: 🟠 High

**问题描述**:
- 800 人技术团队，一个 workspace 可能有 100+ 人
- 当前设计：workspace 内所有人权限相同
- HR/客服/合规场景的硬门槛：同一 workspace 内，不同人可看的文档不同

**修复方案**:
1. 增加 `veda_acls` 表：`(workspace_id, path_pattern, account_id, permission)`
2. 在 `FsService` 层检查 ACL
3. 提供 `/v1/acls` API 管理

---

### P1-2. 无 quota / rate limit

**严重程度**: 🟠 High

**问题描述**:
- 单 workspace、单 key、单 IP 都没有限制
- 一个失控 agent 循环可导致：
  - Embedding API 预算打爆
  - Worker 队列塞满，阻塞所有 tenant
  - MySQL 连接池耗尽

**修复方案**:
1. 添加 `veda_quotas` 表：存储空间、文件数量、API 调用限制
2. 在 `FsService` 层检查 quota
3. 实现基于 Redis 的 sliding window rate limiter
4. 配置项：`--rate-limit-requests-per-minute=100`

---

### P1-3. `CorsLayer::permissive()` 允许任意 origin

**严重程度**: 🟠 High

**位置**: `main.rs:116`

**问题描述**:
- 任何 origin 都能从浏览器命中 API
- CSRF 风险

**代码证据**:
```rust
// main.rs:116
.layer(CorsLayer::permissive())
```

**修复方案**:
```rust
let allowed_origins = cfg.allowed_origins.iter()
    .map(|s| s.parse::<HeaderValue>().unwrap())
    .collect::<Vec<_>>();
let cors = CorsLayer::new()
    .allow_origin(allowed_origins)
    .allow_methods([Method::GET, Method::POST, Method::PUT, Method::DELETE])
    .allow_headers([AUTHORIZATION, CONTENT_TYPE]);
```

---

### P1-4. Login 无防爆破保护

**严重程度**: 🟠 High

**位置**: `routes/account.rs:84-102`

**问题描述**:
- 无 rate limit
- 无 account lockout
- Argon2 慢但暴力可行（~10ms/hash，1000 req/s = 60M attempts/day）

**修复方案**:
1. 添加基于 IP 的 rate limit（5 failures/minute → lockout 15min）
2. 记录失败次数到 Redis
3. 可选：CAPTCHA after 3 failures

---

### P1-5. JWT 不可撤销

**严重程度**: 🟠 High

**位置**: `auth.rs:48-58`

**问题描述**:
- 只验证 `exp` + `iss`，无 `jti` 黑名单
- 无 `kid` 轮换机制
- `jwt_secret` 一旦泄露，所有已签 token 都活到 exp

**修复方案**:
1. 添加 `veda_jwts_blacklist` 表存储 revoked `jti`
2. 或使用 short-lived JWT (15min) + refresh token
3. 支持 `kid` 轮换：多 secret 同时有效

---

### P1-6. Workspace-key 路径下 account_id 为空字符串

**严重程度**: 🟠 Medium

**位置**: `auth.rs:175`

**问题描述**:
- 使用 workspace key 认证时，`account_id` 设为空字符串
- 审计日志中"谁做了什么"归因断了

**代码证据**:
```rust
// auth.rs:175
(ws.workspace_id, String::new(), read_only)  // ← account_id = ""
```

**修复方案**:
1. 在 `veda_workspace_keys` 表添加 `created_by` 字段
2. 或查询 workspace 所属 account

---

### P1-7. 搜索/列表缺少合理上限

**严重程度**: 🟠 High

**位置**: `search.rs:25`, `fs.rs:52-54`

**问题描述**:
- `/v1/search` 默认 limit=10，但可传任意值
- Milvus 硬上限 16383，但不应该在业务层接近这个值
- `list_dir` 全表返回，无分页
- `list_workspaces` 全表返回

**代码证据**:
```rust
// search.rs:25
let limit = req.limit.unwrap_or(10);  // 没有 max cap

// fs.rs:52-54 (list_dir)
let entries = state.fs_service.list_dir(&auth.workspace_id, "/").await?;
// 无分页参数
```

**修复方案**:
1. search API: `limit = limit.unwrap_or(10).min(100)`
2. list_dir: 添加 `?page=&page_size=` 参数
3. list_workspaces: 添加分页

---

### P1-8. Worker 单进程内嵌

**严重程度**: 🟠 Medium

**位置**: `main.rs:95-112`

**问题描述**:
- Server 和 Worker 共享进程
- 无法独立扩容
- 多副本部署时多个 worker 会重复 claim（靠 SKIP LOCKED 兜底）
- 生产应 server / worker 分离部署

**代码证据**:
```rust
// main.rs:95-112
let worker_handle = if cfg.worker.enabled {
    let w = Worker::new(...);
    Some(tokio::spawn(async move { w.run(shutdown_rx).await; }))
} else { None };
```

**修复方案**:
1. 提取 worker 为独立 binary：`veda-worker`
2. 支持 `--worker-count` 参数
3. K8s 部署时使用 Deployment 管理副本数

---

### P1-9. 无 Prometheus metrics / 无 DLQ 可观测性

**严重程度**: 🟠 High

**问题描述**:
- 没有 metrics 导出
- Operator 只能直接 SQL 查看队列状态
- 缺少关键指标：
  - 队列深度 / 积压 / 失败率 / dead 数量
  - 每 workspace token 用量
  - Embedding API 延迟 / 费用
  - MySQL 连接池使用率

**修复方案**:
1. 集成 `axum-prometheus` 或手动实现 `/metrics`
2. 暴露核心指标：
```rust
lazy_static! {
    static ref QUEUE_DEPTH: IntGauge = ...;
    static ref EMBED_LATENCY: Histogram = ...;
}
```

---

### P1-10. 单 Milvus 集合，filter-based 多租户

**严重程度**: 🟠 Medium

**位置**: `milvus.rs:352-387`

**问题描述**:
- 所有 tenant 数据在同一 collection，靠 `workspace_id` filter 隔离
- 已知 cliff：
  - Buggy filter 可能漏数据/越权
  - 备份恢复无法按 tenant 粒度
  - ANN 索引在跨 tenant 数据上召回受污染（一个 tenant 的大量相似文档影响其他 tenant）

**代码证据**:
```rust
// milvus.rs:360
let filter = format!("workspace_id == {ws}");
```

**修复方案**:
1. 短期：确保 filter 逻辑正确，添加集成测试
2. 长期：考虑 partition-based 隔离（Milvus 支持 partition）

---

### P1-11. 全文搜索使用 LIKE，无倒排索引

**严重程度**: 🟠 Medium

**位置**: `milvus.rs:433-483`

**问题描述**:
- Milvus `LIKE` 是 pattern matching，不是真正的全文搜索
- 大数据量下性能差（O(n) scan）
- 无 TF-IDF / BM25 排序

**修复方案**:
1. 集成 Elasticsearch / Meilisearch 做全文搜索
2. 或使用 Milvus 2.5+ 的 Full-Text Search 功能
3. 当前实现可标注为 "basic fulltext"，文档说明限制

---

### P1-12. 大文件读取内存风险

**严重程度**: 🟠 Medium

**位置**: `fs.rs:434-450`

**问题描述**:
- `read_file_range` 先读全量内容再切片
- 大文件可能 OOM

**代码证据**:
```rust
// fs.rs:441
let content = self.read_file(workspace_id, raw_path).await?;
let bytes = content.as_bytes();  // ← 全量加载到内存
```

**修复方案**:
1. 对 chunked 文件，只读取需要的 chunks
2. 添加内存使用限制

---

## P2 - 规模化/自助化前的债务

### P2-1. 无 Connector / Import Inbox

**问题**: 只能 `veda cp`，无法对接 S3/Git/Confluence 等

**修复**: 设计 Import API，支持 webhook 触发

---

### P2-2. 无 OpenAPI / 无 SDK

**问题**: Partner 拿到的就是 curl 示例

**修复**:
1. 使用 `utoipa` 生成 OpenAPI spec
2. 使用 OpenAPI Generator 生成 TypeScript/Python SDK

---

### P2-3. 无部署 Artifact

**问题**: Dockerfile / Helm / docker-compose 都没有

**修复**:
```dockerfile
FROM rust:1.78 AS builder
WORKDIR /app
COPY . .
RUN cargo build --release

FROM debian:bookworm-slim
COPY --from=builder /app/target/release/veda-server /usr/local/bin/
CMD ["veda-server", "/etc/veda/config.toml"]
```

---

### P2-4. `migrate()` 启动无脑跑

**严重程度**: 🟡 Medium

**位置**: `main.rs:41`

**问题描述**:
- 每次 server 启动都执行 migrate
- 多副本启动时 race
- 生产应使用独立 migration job

**修复方案**:
1. 添加 `--skip-migrate` 参数
2. 或提取为独立 `veda-migrate` binary

---

### P2-5. Citation 基于 chunk_index，重切就断

**问题**: 引用 `file_id:chunk_index`，内容重切后 index 变化，历史引用失效

**修复方案**:
1. Citation 改为 `file_id:start_line-end_line`
2. 或持久化 chunk 内容 hash，引用指向 hash

---

### P2-6. CollectionSync 未实现

**严重程度**: 🟡 Medium

**位置**: `worker.rs:131-135`

**问题**: Collection 路径的 outbox 集成有断点

**代码证据**:
```rust
// worker.rs:131-135
OutboxEventType::CollectionSync => {
    let msg = "CollectionSync not implemented";
    warn!(task_id = task.id, msg);
    return Err(VedaError::Internal(msg.to_string()));
}
```

---

### P2-7. 无 Session 管理 / 强制登出

**问题**: JWT 无状态，无法强制用户登出

**修复**: 见 P1-5

---

### P2-8. 无审计日志

**问题**: 无法追踪"谁在什么时候对什么做了什么"

**修复方案**:
```sql
CREATE TABLE veda_audit_logs (
    id BIGINT AUTO_INCREMENT PRIMARY KEY,
    workspace_id VARCHAR(36) NOT NULL,
    account_id VARCHAR(36),
    action VARCHAR(32) NOT NULL,
    resource_type VARCHAR(32),
    resource_id VARCHAR(64),
    details JSON,
    ip_address VARCHAR(45),
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    INDEX idx_ws_time (workspace_id, created_at),
    INDEX idx_account (account_id)
);
```

---

### P2-9. 连接池监控缺失

**问题**: sqlx pool 没有 metrics 导出

**修复**:
```rust
// 定期记录
let pool_status = mysql.pool().status();
info!(
    connections = pool_status.num_connections,
    idle = pool_status.num_idle,
    "mysql pool status"
);
```

---

### P2-10. 无分布式追踪

**问题**: 没有 OpenTelemetry 集成

**修复**: 添加 `tracing-opentelemetry` layer

---

### P2-11. file_id 暴露在 API 响应中

**严重程度**: 🟡 Low

**问题**: 可能导致枚举攻击

**修复**: 使用 opaque ID 或签名 ID

---

### P2-12. 敏感信息可能泄露到日志

**问题**: tracing 日志可能包含敏感信息

**修复**: 添加日志脱敏层

---

## 修复优先级建议

### Tier-A：Production Fitness MVP（4-6 周）

| # | 问题 | 预估工时 |
|---|------|----------|
| 1 | Reconciler | 1.5 周 |
| 2 | Health/Ready 端点 | 2 天 |
| 3 | Worker 原子性 + content_hash 跳过 + embed 切批/重试 | 1 周 |
| 4 | Secret 从 env 加载 | 2 天 |
| 5 | Workspace rate limit + quota | 1 周 |
| 6 | Prometheus metrics + DLQ admin | 3 天 |

### Tier-B：接 Partner 前补完（约 4 周）

| # | 问题 | 预估工时 |
|---|------|----------|
| 7 | Doc-level ACL | 1.5 周 |
| 8 | Pagination + limit cap | 3 天 |
| 9 | Login 防爆 + JWT 撤销 | 3 天 |
| 10 | Worker 拆独立进程 | 2 天 |
| 11 | CORS 白名单 | 1 天 |
| 12 | 二进制文件处理 | 3 天 |

### Tier-C：产品化（按 partner 需求驱动）

| # | 问题 | 预估工时 |
|---|------|----------|
| 13 | Connector + Import API | 2 周 |
| 14 | OpenAPI + SDK | 1 周 |
| 15 | Helm chart + migration job | 3 天 |
| 16 | Citation 基于 line range | 3 天 |
| 17 | 审计日志 | 3 天 |
| 18 | 分布式追踪 | 2 天 |

---

## 关键判断

最核心的两条：

1. **P0-1 Reconciler** —— 数据正确性根基
2. **P1-1 Doc-level ACL** —— 多租户隔离硬门槛

这两条不解决，Veda 任何 production 部署都有地雷风险。

---

## 附录：审计方法

- 代码版本：`1b76cd7` (2026-04-28)
- 审计范围：`veda-server`, `veda-core`, `veda-store`, `veda-pipeline`, `veda-fuse`
- 审计方法：静态代码分析 + 架构文档对照
