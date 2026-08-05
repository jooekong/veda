# 公司级上线前加固审查最终稿（2026-06-18）

> 📌 **2026-08-05 状态核对**（逐条对照代码实况）：必修 11 条 **7 修 4 欠**。
> 已修（**2026-06-18 当天即随 `5d755b0` 落地并部署三台**，「基本未落地」的旧表述指的是 06-15 时点）：#1 SQL 规划器硬闸（`engine.rs` SQLOptions 无条件禁 DDL/DML/statements）、#2 内存池+超时（GreedyMemoryPool + SQL_QUERY_TIMEOUT）、#4 key revoke 带 workspace scope、#5 delete/revoke 补平台 authz、#6 platform reqwest 3s 超时、#10 全局 TimeoutLayer + CatchPanicLayer（另 systemd 加固 unit 三台已装）。
> 仍欠，分两类——**真欠账**：#3 CI 不跑任何测试（两套 CI 均 tag-only release，且干净环境 `cargo test` 依赖 gitignored 的 test.toml 会崩）。**Joe 拍板暂不做**（2026-06-18「初期不删数据」）：#7 db workspace 全局配额、#8 删 project drop Milvus collection、#9 insert_rows 行数上限——重启这三条前先向 Joe 确认拍板是否仍有效。#11 install.sh 只读 deploy token 未轮换（repo 已 private，降级接受）。批次 C（审计日志/request-id、list_dir 单层 LIMIT、collection upsert/saga、outbox pending 上限）未动。
> 原始材料稿已归档 `../archive/reviews/`。

- **HEAD**：`469f0c7`
- **来源**：
  - [`review-2026-06-18.md`](../archive/reviews/review-2026-06-18.md)：多智能体安全/健壮性复审，重点补充 apps / platform 新面。
  - [`review-2026-06-18-cursor.md`](../archive/reviews/review-2026-06-18-cursor.md)：Cursor 三维度复审，覆盖健壮性、数据正确性、测试/CI、可观测性与部署。
  - [`review-2026-06-15.md`](review-2026-06-15.md)：上一轮全员开放安全基线。
- **最终定级原则**：这是公司内部项目，**token / 鉴权机制从简**；缺 in-code auth 不单独作为 blocker。但会导致**宿主机写入、节点 OOM、单人拖垮全员、跨租户破坏、凭证泄露、上线后无法回归**的问题仍按上线风险处理。

---

## 总体结论

核心数据路径整体扎实：outbox lease fencing / takeover / dead-letter、worker 幂等、Milvus upsert/delete 重试、文件写入与异步任务同事务入队、path/checksum 边界都没有发现上线 blocker。

真正挡全员开放的是三类问题：

1. **SQL 执行面缺资源和语义边界**：能写宿主机，也能 OOM 打死节点。
2. **apps/db workspace 新面有几处分钟级但真实的破坏性缺口**：key revoke 未按 workspace scope、销毁端点漏外部 authz、删项目漏 drop Milvus collection、db collection 无全局配额。
3. **上线守门缺失**：CI 不跑测试，凭证仍硬编码，部署/日志/超时还没形成生产闭环。

---

## 一、必须修 before 全员开放

> 下表是最终 blocker / near-blocker 清单。多数是分钟级局部改动，建议按一批落地。

| # | 问题 | 位置 | 影响 | 最小修复 |
|---|---|---|---|---|
| 1 | **`/v1/sql` 可写宿主机**：裸 `ctx.sql(sql)` 允许 `COPY ... TO` / `CREATE EXTERNAL TABLE LOCATION`，`read_only` 不进 DataFusion planner | `crates/veda-sql/src/engine.rs:55,116` | 以 `veda` 用户写宿主机文件。当前生产 unit 不是 root，但 `ExecStart=/data/veda/bin/veda-server`，若该路径可写，覆盖二进制 + 重启仍可 RCE | 改 `ctx.sql_with_options(sql, SQLOptions::new().with_allow_ddl(false).with_allow_dml(false).with_allow_statements(false))`，无条件禁 DDL/DML/statements |
| 2 | **`/v1/sql` 无内存池 / 无查询超时 / 全量物化** | `crates/veda-sql/src/engine.rs:55,119`，`routes/sql.rs` Arrow JSON 再缓冲 | 一条大 cross join / ORDER BY 可触发无界内存增长，OOM kill 整节点 | 用 bounded memory pool（如 256-512MB）构造 DataFusion runtime；`df.collect()` 包 `tokio::time::timeout(30s)`；补大查询中止测试 |
| 3 | **CI 不跑测试，且当前干净环境 `cargo test` 会因新增 apps 测试 panic** | CI 流水线 release-only；apps 新测试未挂 `#[ignore]` | 改坏路由/隔离/资源上限可 green 合入；这是上线回归守门 blocker | 先修漏挂 `#[ignore]`，再加至少 `cargo test` 单测 job；随后补真实 MySQL/Milvus 的集成 job |
| 4 | **apps key revoke 缺 workspace scope**：handler 已校验项目归属，但 store `UPDATE ... WHERE id=?` 不带 `workspace_id` | `crates/veda-server/src/routes/apps.rs:549-554`，`crates/veda-store/src/mysql.rs:3099-3105` | 持任一项目、若知道另一 key UUID，可吊销其他 workspace 的 `wk_`。UUID 不易枚举，但这是明确的跨租户破坏性正确性 bug | `revoke_workspace_key(id, workspace_id)`，SQL 改 `WHERE id=? AND workspace_id=?`；0 行返回 `NotFound` |
| 5 | **apps 销毁端点漏外部 authz**：`delete_app_project` / `revoke_app_key` 不收 `GatewayUser`，不调 `platform::authorize` | `crates/veda-server/src/routes/apps.rs:367-384,549-555` | 在 `VEDA_PLATFORM_BASE` 已配置时，create/update/key-token 走平台策略，但 delete/revoke 可绕过策略。注意仍有 account 归属校验，不是任意人可删 | 两个 handler 加 `GatewayUser` + `platform::authorize(...)`；沿用现有 `workspace-create` action 或拆 destructive action |
| 6 | **外部 platform reqwest 无超时** | `crates/veda-server/src/platform.rs:128,155` | 平台 accept 但不响应时，请求 task 永久挂起；全局无 TimeoutLayer 时放大 | 复用一个带 `timeout(3s)` / `connect_timeout` 的 `reqwest::Client` |
| 7 | **db workspace / Milvus collection 无全局配额** | `crates/veda-server/src/routes/account.rs` create workspace 路径；apps 可自动开户建 db workspace | 每个 db workspace 常驻一个 Milvus collection；循环创建可吃光集群 loaded collection 预算，影响全员 | 建 db workspace 前做**全局** active db workspace / collection 计数，超过上限返回 429；上线前 spike 目标 Milvus loaded collection 上限 |
| 8 | **删 apps project 不 drop 对应 Milvus collection** | `crates/veda-server/src/routes/apps.rs:383` 仅 `delete_workspace` | create/delete 循环会留下 Milvus collection，绕过 #7 的 active 计数，最终耗尽命名空间/collection 预算 | delete db project 后幂等 `drop_collection(vector_collection_name(ws.id))`；失败记录 warn，必要时返回 500 而不是静默泄漏 |
| 9 | **collection `insert_rows` 无行数上限** | `crates/veda-server/src/routes/collection.rs:85-95`，`core/service/collection.rs:112-138` | 64MB body 可携带大量行并同步调用 embedding，上游共享资源被单请求长期占用 | handler 前置 `MAX_ROWS=500`，对齐 vectors upsert |
| 10 | **全局请求超时缺失** | `crates/veda-server/src/main.rs:328-331` | SQL、platform、embedding/Milvus 异常慢时没有墙钟兜底 | 给非长连接 API 加 `TimeoutLayer(30s/60s)`；**当前 `/v1/events` 是 SSE 长连接，必须单独挂不超时 router 或显式豁免** |
| 11 | **硬编码凭证仍在仓库/二进制/HTTP 中暴露** | `install.sh:27`，`routes/mod.rs:29,166`，`scripts/loadtest/*.py` | GitLab deploy token 被 `include_str!` 编进 server 二进制并通过 `GET /install.sh` 无鉴权返回；Milvus `rw_public` test 凭证仍硬编码 | 轮换 GitLab token + Milvus `rw_public`；`install.sh` token 模板化或默认走公开 release；脚本改强制 env |

### 重要修正

- SQL 写宿主机不是“root 直接 RCE”：当前 systemd unit 使用 `User=veda`。更准确的风险是 `veda` 用户可写范围内的文件破坏，尤其当前部署若让 `/data/veda/bin/veda-server` 可写，则可覆盖二进制等待重启。
- systemd 沙箱不是 #1 的根因修复：根因只能靠 `SQLOptions` 禁 DDL/DML/statements。部署沙箱仍应做，但如果 `ReadWritePaths=/data/veda` 包含 bin 目录，仍挡不住覆盖二进制。最终部署应只允许数据目录写入，例如 `ReadWritePaths=/data/veda/data`。
- `/v1/events` SSE 路由仍存在，不能把 TimeoutLayer 粗暴套到整个 router。

---

## 二、强烈建议同批修

| 问题 | 位置 | 建议 |
|---|---|---|
| **审计日志缺 actor / workspace 归属** | `crates/veda-server/src/obs/mod.rs:40-66`，`routes/sql.rs` | 至少对 `/v1/sql`、delete/revoke、create key、workspace/project mutation 打结构化 `info!`：`account_id/workspace_id/route/status/latency/request_id`；SQL 记录摘要、行数、耗时 |
| **生产 INFO 下无 per-request 日志、无 request-id** | `TraceLayer::new_for_http()` 默认 INFO 不打每请求日志 | 加 request-id middleware；5xx/慢请求 INFO/WARN 可 grep |
| **无 CatchPanicLayer** | `main.rs:328-331` | handler panic 不会杀进程，但会 reset 连接、metrics/span 不完整；加 `tower-http` `catch-panic` |
| **生产部署 unit 使用混乱** | `scripts/deploy/veda-server.service` 未加固；`deploy/systemd/veda-server.service` 加固版未被主 deploy 引用 | 统一到一个生产 unit：非 root、`ProtectSystem=strict`、`ProtectHome=true`、`PrivateTmp=true`、`ReadWritePaths` 只给数据目录、`MemoryMax` 有上限 |
| **`list_dir` 无 LIMIT / 分页** | `crates/veda-store/src/mysql.rs` `list_dentries` | 大目录一次全量物化；加 `LIMIT` 或 cursor 分页 |
| **structured collection `insert` 非 `upsert`** | `crates/veda-core/src/service/collection.rs:155-157` | 重复 id 行为与 vectors 面不一致；改为 upsert 或先按 id 去重 |
| **collection delete saga 顺序可优化** | `core/service/collection.rs:101-106` | 当前先 drop Milvus 再删 MySQL；DB 中途失败会卡住名字。建议先删 schema 行再 best-effort drop Milvus，或记录可重试 tombstone |
| **ChunkSync 文件已删分支未清 summary** | `worker.rs` 删除 chunks 分支 | 可能留下同租户孤儿 summary 向量；补 delete summary |
| **outbox 全局 FIFO** | `mysql.rs` claim `ORDER BY id ASC` | 一个 bulk 租户拖慢其他租户 embedding/summary 新鲜度；先加 per-workspace pending 上限，复杂调度器延后 |
| **`rustls-webpki` advisory** | `Cargo.lock` | 路径基本不可达，但升级便宜：`cargo update -p rustls-webpki` |
| **apps 分页 offset u32 溢出** | `apps.rs:281,651` | `(page - 1) * size` 改 i64/saturating，避免 release wrap |
| **workspace code 未严格字符集校验** | `apps.rs:175` `require_workspace` | 收紧为 `[A-Za-z0-9_-]+`，也避免拼 platform URL path 时出现奇怪路径 |

---

## 三、按“token 从简”的部署硬约束

这些不作为代码 blocker，但必须写入上线 checklist：

- `:3000` 不允许全员直连，必须经网关或安全组限制来源。
- apps 面如果对 AI Workbench 开放，`VEDA_PLATFORM_BASE` 必配；当前未配时 `platform::authorize` fail-open，这是 dev 便利，不是生产默认。
- `/v1/accounts/anonymous`、apps 自动开户、`GET /install.sh` 等无鉴权入口必须由网关/网络边界兜住。
- TLS 在外层代理终结；server 内不做 TLS 可以接受，但部署文档要明确 token 不应在不可信网络明文传输。
- 新增 `config/server.toml.example`，列出 `metrics_token`、`allowed_origins`、`dev_mode=false`、`drain_secs`、`VEDA_PLATFORM_BASE`、OTLP、外部服务超时等生产键。

---

## 四、暂不修 / 明确降级

- **全局 ConcurrencyLimit / LoadShed**：先补 SQL 内存/超时、route row limits、platform timeout，再用压测数据定并发阈值。盲加全局并发闸可能误伤 SSE、长上传和 worker 回调路径。
- **后台 Milvus GC reaper**：先在删 db project 时内联幂等 drop collection；reaper 是后续治理，不是最小上线修复。
- **outbox WFQ 调度器**：当前 FIFO 影响异步新鲜度，不影响同步读写正确性；先加 pending 上限，避免引入复杂调度器。
- **delete saga “无回滚”**：定性降级。当前失败可重试/自愈，主要是顺序和可观测性问题。
- **FUSE writeback `base_rev` 丢写**：writeback 是 opt-in 实验特性；若暂不对公司开放 writeback，可降级为近期修。
- **安全响应头**：当前主要 API 面，不是浏览器渲染站点；低优先。
- **`quinn-proto` / `rsa` advisory**：`quinn-proto` 未编进 server；`rsa` 仅 MySQL 握手可信段且上游无 fix，记录接受即可。
- **`dev_mode` 默认不安全**：已修正为默认 `false`，开启会 WARN；06-15 的旧定性不再成立。

---

## 五、测试与 CI 最小门槛

上线前至少补以下守门：

1. **普通 CI**：`cargo test` 必跑。先修 apps 测试漏 `#[ignore]` 的干净环境 panic。
2. **SQL 回归**：`COPY ... TO` / `CREATE EXTERNAL TABLE` 被拒；超大 join/order 查询被 timeout 或 memory pool 中止。
3. **apps 隔离回归**：A 项目路径下用 B 的 `key_id` revoke 返回 `NotFound`，且 B key 状态不变。
4. **apps destructive authz 回归**：平台拒绝时 delete project / revoke key 返回 403。
5. **collection row cap**：超过 500 rows 返回 413/400，不调用 embedding。
6. **db collection 配额回归**：超过全局 ceiling 返回 429；delete db project 后 collection 被 drop 或失败可见。
7. **真实依赖集成 job**：MySQL + Milvus + embedding test env，`NO_PROXY='*' --test-threads=1`，覆盖 vectors/fs/collection/apps 关键 HTTP 路径。

---

## 六、最小上线批次

建议按三批推进：

### 批次 A：最高 ROI，半天内

- SQL `SQLOptions`
- SQL bounded memory pool + timeout
- platform reqwest timeout
- collection `insert_rows` row cap
- apps key revoke workspace scope
- apps delete/revoke 补 platform authz
- 修 apps 测试 `#[ignore]`，让 `cargo test` 先能跑

### 批次 B：上线门禁

- 加 `cargo test` CI job
- db workspace / Milvus collection 全局配额
- delete db project drop collection
- 凭证轮换 + install.sh token 模板化
- 生产 systemd unit 统一加固，`ReadWritePaths` 只给数据目录
- 部署文档写清网关/安全组/TLS/`VEDA_PLATFORM_BASE`

### 批次 C：近期加固

- `/v1/sql` 与 mutation 审计日志、request-id
- CatchPanicLayer
- `list_dir` LIMIT / 分页
- collection upsert / delete saga 顺序优化
- `rustls-webpki` 升级
- outbox pending 上限

---

## 七、最终拍板表

| 项 | 类型 | 优先级 | 备注 |
|---|---|---|---|
| SQL 禁 DDL/DML/statements | 代码 | P0 | 最高 ROI，直接修 |
| SQL memory pool + timeout | 代码 | P0 | 与上同文件 |
| CI 跑 `cargo test` | CI | P0 | 先修漏挂 ignore |
| apps key revoke scope | 代码 | P0 | 跨租户破坏性 bug，分钟级 |
| platform reqwest timeout | 代码 | P0 | 防挂死 |
| collection rows cap | 代码 | P0 | 防共享 embedding 被单请求占住 |
| db collection 全局配额 + delete drop | 代码/容量 | P0 | 需要 Milvus 容量 spike |
| delete/revoke platform authz | 代码 | P1 | 若 apps 面上线给 AI Workbench，按 P0 |
| 凭证轮换 | ops | P0 | 按已泄露处理 |
| systemd 加固 | ops | P0 | 注意 bin 目录不可写 |
| 网关/安全组硬约束 | ops/doc | P0 | token 从简的前提 |
| 审计日志/request-id | 代码 | P1 | 出事溯源 |
| CatchPanicLayer | 代码 | P1 | 干净 500 + metrics 完整 |
| list_dir 分页 | 代码 | P1 | 大目录内存峰值 |

> **最终结论**：可以不把 token 鉴权做重，但必须把 SQL 执行边界、apps 新面的破坏性小洞、资源配额、CI、凭证轮换和部署边界补齐。补完批次 A+B 后，再做一次真实依赖集成回归，可进入公司内部灰度。
