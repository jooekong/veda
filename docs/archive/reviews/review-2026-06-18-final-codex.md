# veda 上线前最终综合审查（Codex，2026-06-18）

## Verdict

当前 **不具备 launch-ready 条件**。我把真正的 hard blocker 计为 **4 个**：`/v1/sql` 宿主机写/RCE 原语、`/v1/sql` 无资源边界导致整节点 OOM、CI 当前干净 `cargo test` 会误跑真实依赖测试并失败、生产文档仍安装未加固 systemd unit。除此之外还有 5 个分钟级 must-fix（SSE timeout carve-out、apps 两个销毁端点 authz 缺口、跨租户 revoke scope、平台 HTTP timeout、删除 app project 后 orphan Milvus collection），都应该同批处理，因为修复小且直接降低隔离/运维风险。

## Must-Fix Table

| # | Issue | File:Line | Severity | Fix-Cost | Minimal Fix |
|---|---|---|---|---|---|
| 1 | `/v1/sql` SQL COPY filesystem write：DataFusion 仍走裸 `ctx.sql(sql)`，`read_only` 只约束 UDF，不约束 planner；`COPY ... TO` / external table 可写宿主机文件，覆盖 service binary 后重启即 RCE | `crates/veda-server/src/routes/sql.rs:13-24`; `crates/veda-sql/src/engine.rs:55,115-120` | 🔴 blocker | 分钟 | 改 `ctx.sql_with_options(sql, SQLOptions::new().with_allow_ddl(false).with_allow_dml(false).with_allow_statements(false))`，无条件禁止 SQL DDL/DML/statements；保留 `SELECT` 与 `veda_*` UDF 自身权限闸 |
| 2 | `/v1/sql` 无内存池 / 无执行超时 / 全量物化：`SessionContext::new()` + `df.collect()` + route 再 Arrow JSON 全量缓冲，一条大 join/order 可拖垮整节点 | `crates/veda-sql/src/engine.rs:55,119-120`; `crates/veda-server/src/routes/sql.rs:27-39` | 🔴 blocker | 小 | 给 DataFusion runtime 配有界 memory pool；`collect()` 包 `tokio::time::timeout(30s)`；必要时把超时/资源耗尽映射成稳定错误 |
| 3 | 全局请求超时缺失，但修复时必须排除 `/v1/events`：该路由是真 SSE，套 30s `TimeoutLayer` 会主动杀掉 FUSE/CLI event stream | `crates/veda-server/src/main.rs:328-331`; `crates/veda-server/src/routes/events.rs:7,29-30` | 🟡 medium | 分钟 | 给普通 HTTP route 加墙钟 timeout；把 `/v1/events` 放到无 timeout 分支，或用 predicate/layer 分流只对非 SSE 路由生效 |
| 4 | apps/platform 两个销毁端点 zero-authz：create/update/create-key/getToken/create-dataset 已调用 `platform::authorize`，但 `delete_app_project` 与 `revoke_app_key` 没有 `GatewayUser` 也不调 authorize | `crates/veda-server/src/routes/apps.rs:234-243,319-325,367-383,440-446,531-538,549-554,591-597` | 🟡 medium | 分钟 | 两个 handler 增加 `gw: GatewayUser`，先调用 `authorize(gw.cookie(), "workspace-create", &workspace, gw.user_name()).await?`，再做现有归属校验 |
| 5 | Cross-tenant key revoke missing `workspace_id` scope：`get_workspace_key_token` 正确按 `id + workspace_id` 查，`revoke_workspace_key` 只按裸 key id 更新；apps revoke 只传 `key_id` | `crates/veda-store/src/mysql.rs:3024-3033,3099-3101`; `crates/veda-server/src/routes/apps.rs:549-554` | 🟠 high | 分钟 | `revoke_workspace_key(id, workspace_id)`，SQL 改 `WHERE id = ? AND workspace_id = ?`；0 affected rows 返回 `NotFound` |
| 6 | CI effectively broken：同一测试文件只有旧 `vectors_http_e2e_suite` 有 `#[ignore]`；两个新增 apps e2e 缺 `#[ignore]`，干净 `cargo test` 会进 `build_test_app()` 连接 MySQL/Milvus | `crates/veda-server/tests/vectors_http_test.rs:334-335,548-550,738-747` | 🔴 blocker | 分钟 + CI 小改 | 先给两个 apps e2e 补 `#[ignore]`；再新增显式真实依赖 job（MySQL/Milvus，`-- --ignored --test-threads=1`）跑数据面/隔离回归 |
| 7 | systemd hardening 用错 unit：`deploy/systemd/veda-server.service` 已加固但孤儿；`docs/deploy.md` 安装 `scripts/deploy/veda-server.service`，其中 `ProtectSystem` / `ReadWritePaths` 注释且无 `MemoryMax` | `deploy/systemd/veda-server.service:23-44`; `scripts/deploy/veda-server.service:21-24`; `docs/deploy.md:41-44` | 🔴 blocker (ops) | 分钟 | 部署改用 hardened unit，或把 socket activation 需要的字段合并进 hardened unit；关键是 `ProtectSystem=strict`、`ReadWritePaths=/data/veda/data`、`MemoryMax=4G` |
| 8 | `platform.rs` reqwest no timeout：平台 authz/name resolve 每次 `Client::new().send().await`，平台 accept 后不响应会永久占住请求任务 | `crates/veda-server/src/platform.rs:127-133,154-159` | 🟡 medium | 分钟 | 建一个共享 `LazyLock<reqwest::Client>`，配置 `.timeout(Duration::from_secs(3))`；`authorize` fail-closed，`resolve_workspace_name` 继续返回 `None` |
| 9 | `delete_app_project` orphan Milvus collection：handler 只 `delete_workspace`，store 只归档 workspace + revoke keys，不 drop db workspace 的 Milvus collection | `crates/veda-server/src/routes/apps.rs:375-383`; `crates/veda-store/src/mysql.rs:2781-2800`; `crates/veda-server/src/routes/account.rs:412-439` | 🟡 medium | 分钟 | 在 delete handler 读到 `ws.kind == Db` 时，删除后调用 `drop_collection(vector_collection_name(&ws.id))`；drop 失败记录 warn，不引入后台 GC |

## Strongly Recommended (cheap batch)

- **db workspace / Milvus collection 全局配额**：`create_workspace_under` 对 `WorkspaceKind::Db` 直接建 workspace、dataset、Milvus collection（`crates/veda-server/src/routes/account.rs:361-395`），没有 active db workspace / collection ceiling。上线前至少加一个全局上限常量或配置，超过返回 quota error；容量 spike 可以后补，但先有闸门。
- **collection `insert_rows` 行数上限**：handler 直接把 `req.rows` 交给 service，service 对所有 rows 组 texts 并同步 embedding（`crates/veda-server/src/routes/collection.rs:85-95`; `crates/veda-core/src/service/collection.rs:112-138`）。对齐 vectors 面，加 `MAX_ROWS_PER_INSERT = 500`。
- **审计与 request-id 最小切片**：`track_http` 只有 route/method/status metrics（`crates/veda-server/src/obs/mod.rs:40-66`），`/v1/sql` route 不记录 actor/workspace/sql 摘要（`crates/veda-server/src/routes/sql.rs:17-25`）。先给 `/v1/sql`、apps mutation、5xx/慢请求加结构化 `info!`，不做复杂审计系统。
- **凭证卫生**：两份报告都指出 `install.sh` / loadtest 脚本凭证问题；这是 ops hygiene，不是“token 逻辑从简”。轮换已出现过的 token，并让安装脚本默认走无私有 token 路径。
- **`list_dir` 单层目录 LIMIT / 分页**：`list_dentries` 当前 `fetch_all` 无 LIMIT（`crates/veda-store/src/mysql.rs:924-931`）。不是 hard blocker，但和 FUSE 大目录一起会制造可用性尖峰；先加保守上限或分页参数。

## Defer / Over-Engineered / False Positives

- **Global ConcurrencyLimit trio**：我的取舍是 **现在只做 TimeoutLayer，且必须排除 `/v1/events`；不把 `GlobalConcurrencyLimitLayer + LoadShedLayer + CatchPanicLayer` 作为 launch blocker 打包上**。理由：A 对“无压测先上全局并发闸”的反对成立，MySQL pool 已提供部分背压，错误的全局闸会把长连接/SSE/慢外部依赖混在一个池里；Cursor 对“需要墙钟 timeout”的判断也成立，所以拆开做。`CatchPanicLayer` 可以顺手加，但不是 blocker。
- **collection insert→upsert**：**延后**。`insert_rows` 现在用 insert 的重复 id 语义确实不如 vectors upsert 稳，但它主要是同租户自伤/语义选择，不是跨租户或宿主机风险；先修 row limit，再决定 structured collection 要不要 last-wins。
- **delete-saga**：**拒绝引入 saga/reaper 子系统**。collection delete 的“drop Milvus 再删 MySQL”窗口可以以后用简单换序或 retry 改；apps project delete 的 orphan collection 是 must-fix，但最小正确修法是 inline drop collection，不是后台 GC。
- **quinn-proto audit**：false positive。`cargo tree -i quinn-proto -p veda-server` 无依赖输出，未编进 `veda-server`。
- **rsa audit**：接受/记录，不作为 blocker。`rsa v0.9.10` 经 `sqlx-mysql` 进入（`cargo tree -i rsa -p veda-server`），但暴露面是可信 MySQL 握手，不是 tenant-controlled decrypt oracle。
- **reconciler-grace 0 panic / 计数器持久化**：false positive。当前是按需 reconcile，不是后台循环（`crates/veda-server/src/reconciler.rs:12-19`; `crates/veda-server/src/routes/reconcile.rs:1-11`），main 里也明确 `grace_passes=0`（`crates/veda-server/src/main.rs:106-110`）。
- **`usize::MAX` company envelope**：过度修复。`to_bytes(body, usize::MAX)` 包的是 apps middleware 里 server 自己生成的 response body（`crates/veda-server/src/routes/apps.rs:674-680`），list page size 已 clamp 到 200（`crates/veda-server/src/routes/apps.rs:50-55`），不是攻击者上传 body。
- **`dev_mode` 默认 true / silent start**：obsolete。`dev_mode` 用 `#[serde(default)]`，默认 false（`crates/veda-server/src/config.rs:23-30`），且已有 `dev_mode_defaults_off` 单测（`crates/veda-server/src/config.rs:559-563`）。

## What Each Reviewer Uniquely Caught / Corrected

| Source | Final synthesis |
|---|---|
| Cursor corrected A on SSE/TimeoutLayer | A 的“无 SSE、TimeoutLayer 不用 carve-out”错误；`/v1/events` 是真实 SSE（`events.rs:7,29-30`）。最终结论：加 timeout 但必须排除 SSE。 |
| Cursor corrected A on CI `#[ignore]` | A 的“data-plane HTTP tests 都 ignored”错误；两个新增 apps e2e 没有 `#[ignore]`（`vectors_http_test.rs:548-550,738-747`）。最终结论：CI 当前有效性不成立，是 hard blocker。 |
| Cursor corrected A on systemd hardening | Cursor 抓到两个 unit 分叉：加固版在 `deploy/systemd`，部署文档使用 `scripts/deploy` 未加固版。最终结论：切到 hardened unit 是 cheap defense-in-depth，也是上线前 ops blocker。 |
| Cursor corrected A on `dev_mode` | A 的旧担忧已过期；默认 false 且有单测。最终结论：从 findings 移除。 |
| A caught Cursor missed: cross-tenant key revoke | `revoke_workspace_key` 裸 key id 更新是真问题；利用需要知道 victim key UUID，所以不是开箱即用攻击，但修复 1 行，必须做。 |
| A caught Cursor missed: `platform.rs` timeout | 平台 authz/name resolve 无 reqwest timeout，网关半开会挂请求任务；必须加共享 client timeout。 |
| A caught Cursor missed: orphan Milvus collection deletion | `delete_app_project` 只归档 workspace，不 drop db collection；必须用 inline drop 修，不需要后台 reaper。 |
