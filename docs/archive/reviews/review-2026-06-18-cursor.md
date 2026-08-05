# 上线前加固审查（2026-06-18，Cursor 多维度复审）

- **HEAD**：`469f0c7`（基线 06-15 审查时为 `be35ffa`，期间 ~19 个 commit 均为新增 apps/AI Workbench 控制面，本维度涉及的核心能力逐字节未变）
- **方法**：3 个并行只读维度 agent —— ①健壮性与可用性 ②数据正确性与可靠性 + 测试/CI ③可观测性与运维部署。各自以 [`review-2026-06-15.md`](../../reviews/review-2026-06-15.md) 为基线独立复核（验证仍成立 / 已修 / 定性修正）+ 补充新发现，作者综合去重并重新定级。
- **范围与约束**：公司**内部项目**，**token / 鉴权机制相关安全逻辑从简**；重点是**生产就绪度**——健壮性、可用性、数据正确性、可观测性、部署运维。
- **与 06-15 的关系**：06-15 是"已认证但不可信内部员工"威胁模型的**安全向**审查（跨租户/宿主机逃逸/鉴权缺口/凭证）；本次换**"上线就绪"**视角，淡化鉴权机制、聚焦工程化加固。会导致**进程崩溃 / 单人拖垮全员 / 宿主机被破坏 / 出事无法溯源**的非鉴权问题仍是重点。

---

## 总体结论

> **核心数据正确性是扎实的，默认数据路径无 blocker**：outbox 状态机（lease fencing / takeover / 双路径死信 / 指数退避）、worker 幂等（level-triggered 重读 + watermark）、reconciler（grace passes 防快照竞态）、Milvus 幂等写（upsert/delete 走重试、insert 走 no-retry）、文件写入与 ChunkSync/SummarySync 同 MySQL 事务入队、path/checksum 边界——逐模块读码确认都正确。
>
> 真正卡上线的不是"丢数据"，而是三类工程化缺口：**① SQL 引擎能写宿主机 / 打爆内存；② 没有资源闸门和限流，单人即可拖垮全员；③ CI 根本不跑测试**。鉴权按"从简"要求处理：靠部署隔离 + 凭证轮换兜底，不改代码。

---

## 一、必修（上线前 blocker）

| # | 问题 | 位置 | 为什么内部项目也得修 | 成本 |
|---|---|---|---|---|
| 1 | **`/v1/sql` 可写/覆盖宿主机任意文件**：`COPY ... TO '/path'` 以 server uid 落盘（只读 key 也挡不住，`read_only` 只进 `FsUdfContext` 不进 planner） | `veda-sql/src/engine.rs:55,116`；入口 `veda-server/src/routes/sql.rs:24` | 一条误写/手滑的 SQL 就能覆盖 `~/.ssh/authorized_keys`、`/etc/cron.d/*`、systemd unit；prod 若以 root 跑即 RCE。**全场最高 ROI** | `sql_with_options` 关 ddl/dml/statements，~6 行零副作用 |
| 2 | **`/v1/sql` 无内存池 / 无执行超时 / 全量物化** | `engine.rs:55`（`RuntimeEnv::default()` = `UnboundedMemoryPool`）、`:116,119`（`df.collect()`） | 一条 `files a,b,c` 自连接，**cross join 中间结果不受 `files` 10 万行扫描上限保护** → 永不中止 → OOM 杀整节点；即便不到 OOM 也打满 CPU | `RuntimeEnvBuilder` + `GreedyMemoryPool(256MB)` + `tokio::time::timeout(30s)`，小 |
| 3 | **CI 不跑任何测试**，且当前 `cargo test` 在干净环境**直接 panic**（审查窗口内新增 2 个 apps 测试漏挂 `#[ignore]`） | 两条流水线均 release-only；数据面 HTTP 测试多为 `#[ignore]` | 本次要兜的就是"回归守门"。现状连单测都没人跑，改坏路由能 green 合入 | 补 `#[ignore]`（分钟级）→ 加跑真实 MySQL/Milvus 的 `cargo test` job |
| 4 | **生产 systemd 用沙箱被注释的未加固 unit**，完整加固版（`ProtectSystem=strict`+`ReadWritePaths`+`MemoryMax`）反而闲置无人引用 | `scripts/deploy/veda-server.service:21-24` vs `deploy/systemd/veda-server.service:36-44` | 零代码就能把 #1/#2 的爆炸半径锁到单目录 + 兜 OOM，是 #1 的廉价纵深防御 | ops 动作，deploy 改用加固 unit（确认 `/etc/ddmc/env.yaml` 仍可读） |

> #1 + #2 改动都局限在 `engine.rs` 一个文件、风险极低，建议同批落地，并补"`COPY ... TO` 返 Err / 超大查询被超时中止"回归测试。

### #1 修复参考

```rust
use datafusion::sql::parser::SQLOptions;
let opts = SQLOptions::new()
    .with_allow_ddl(false)
    .with_allow_dml(false)
    .with_allow_statements(false);
let df = ctx.sql_with_options(sql, opts).await?;
```

`SELECT` + `veda_*` 写 UDF（走 Projection + 自身 `read_only` 闸）不受影响。

---

## 二、强烈建议（低成本，与必修同批做掉）

- **审计日志（出事溯源的前提）**：`track_http`（`obs/mod.rs:40-66`）只产出 metrics（route/method/status），**`/v1/sql` 这条最危险路径连操作者都不记**，控制面 mutation 也不记 actor。生产 INFO 级下**每请求零日志、无 request-id**。改成每请求一条结构化 `info!`（actor / workspace_id / sql 摘要 / 行数 / 耗时），或至少 5xx + 慢请求 + 控制面 mutation + `/v1/sql`。**与 #1 同批最划算。**
- **中间件栈三件套**：`main.rs:328-331` 缺 ① `TimeoutLayer`（**排除 SSE `/v1/events`**）② 全局并发闸 `GlobalConcurrencyLimitLayer` + `LoadShedLayer`（过载快速 503）③ `CatchPanicLayer`。三者共享 `tower-http` feature（当前 `Cargo.toml` 仅开 `["cors","trace"]`），一次加齐。注：handler panic 当前默认 unwind 不崩节点，但会 drop 连接、metric 不收尾；DataFusion 是现实 panic 点。
- **db workspace / collection 配额**：全仓无 workspace/collection/account 数量上限。循环建 db workspace → 撑爆**集群级** Milvus collection 天花板（每个常驻不 unload，~1500 即顶，见 `db-workspace-followups.md` A1）→ **杀所有租户**。建前 `COUNT(*) WHERE account_id=? AND kind='db' AND status='active'` 超限 429（~10 行）；**上线前做一次目标 Milvus 的 loaded-collection 容量 spike**（唯一非纯代码项，关系扩展性硬门槛）。apps 面 `create_app_project` 接调用方可控的 `kind`，无网关时放大此风险。
- **`list_dir` 加 LIMIT / 分页**：`mysql.rs:924` `list_dentries` 无上限，单大目录（FUSE 可轻易塞满）`?list` 全量物化。`list_dir_recursive`/glob/grep/SQL files 都已 capped，唯独单层 list 漏了。`insert_rows` 仿 vectors 加 `MAX_ROWS=500`。
- **`cargo update -p rustls-webpki`**：RUSTSEC-2026-0104，实测经 `metrics-exporter-prometheus → hyper-rustls` 编进二进制，但 veda 不做 client-cert/CRL 校验、路径不可达 → 风险低、patch 分钟级。

---

## 三、鉴权 & 凭证（按 "token 从简" 取舍）

- **建议照做（这是凭证卫生，不是 token 校验逻辑，与"从简"无关）**：
  - 轮换硬编码的 Milvus `rw_public` 账号 + GitLab deploy token（`scripts/loadtest/*.py` 4 个脚本 + `install.sh:27`，06-15 起按已泄露处理、至今未轮换；其中 3 个脚本是裸硬编码常量，1 个是 env 默认值兜底）。
  - `install.sh` 的 token 被 `include_str!` 编进 server 二进制（`routes/mod.rs:29`）且经无鉴权 `GET /install.sh`（`:34,166`）服务出去 → 模板化（serve 时从 env 注入）或默认 `SOURCE=github` 免 token。
- **按要求从简、不改代码（靠部署兜）**：
  - apps 控制面 `VEDA_PLATFORM_BASE` 未配时鉴权 **fail-open**（`platform.rs:113`），且 list/get/delete/revoke 未覆盖；`POST /v1/accounts/anonymous` 零鉴权。
  - 处置：必经注入鉴权的网关 + 安全组限 `:3000` 来源，把"必须在网关后 / `VEDA_PLATFORM_BASE` 必配"写成**部署硬约束**（补 `config/server.toml.example` 全键注释 + `docs/deploy.md` 安全节）。若确定只在已鉴权网关后开放，此项不算 blocker。

---

## 四、可延后

- collection `insert` 应改 `upsert`（重复 pk，`service/collection.rs:140`，一行）。
- FUSE writeback `base_rev` 楔死静默丢写 —— **opt-in 实验特性**，除非要对公司开放 writeback。
- delete saga 无回滚（06-15 S-7b）—— 判定**偏重**：drop-Milvus 先行 + drop 幂等、孤儿可 retry 自愈，降级。
- `ServerConfig` derive(Debug) 潜在 secret 泄露（`config.rs`）—— 全仓无实际 `{:?}` 打印点，仅潜在。
- rsa 0.9.10 Marvin 计时侧信道（RUSTSEC-2023-0071）—— veda 是 MySQL 握手加密方、无解密 oracle，不可利用且上游无 fix。
- 无 in-process TLS（token 明文过线，靠外层代理终结）—— 取决于部署形态。
- 登录/建号无限流 + argon2 CPU 放大 —— 随上面的全局限流一起覆盖。

---

## 五、独立判断（未照单全收 06-15）

- **被修正的定性**：
  - `dev_mode` 现已**默认 `false`**（fail-safe），开启打 WARN —— 06-15 "默认 true + 静默启动"**不再成立**。
  - delete saga 无回滚 —— **偏重**，降级到可延后（见上）。
  - "死信状态机零覆盖" —— 修正为 **lease fencing 已测**、dead-letter / backoff 仍空。
  - loadtest 脚本 —— 3 个是**裸硬编码常量**（比 06-15 "env 兜底"描述略差），仅 `milvus_bench.py` 是 env 默认值。
  - quinn-proto RUSTSEC-2026-0037（高危 DoS）—— `cargo tree -i` 证实**未编进 veda-server**。
- **比 06-15 更准**：
  - #2 的 cross join 中间结果**不受** `files` 10 万行扫描上限保护（B2 未点破）。
  - #1 审计缺口具体到 `/v1/sql` 连 actor 都没有。
- **三个维度交叉确认**：06-15 核心结论 3 天内基本原样成立（apps 是新增面，未触及本质能力）。

---

## 六、最小上线集 & 拍板清单

**最小上线集**：#1（SQL 门控）+ #2（SQL 资源上限）+ #3（补 `#[ignore]` / 加 CI）+ #4（换加固 unit）+ 凭证轮换 + 部署写网关硬约束。其中 #1 / #2 / #3补ignore 是分钟~半天级、低风险、可独立验证。

| 项 | 类型 | 成本 | 决策点 |
|---|---|---|---|
| #1 SQL `sql_with_options` | 代码 | 分钟级 | 直接修 |
| #2 SQL memory pool + timeout | 代码 | 小 | 与 #1 同批 |
| #3 补 `#[ignore]` + 加 `cargo test` CI job | 代码 + CI | 小 | 直接修 |
| #4 改用加固 systemd unit | ops | 分钟级 | bound #1，建议直接换 |
| 审计日志 + request-id | 代码 | 半天 | 与 #1 同批 |
| TimeoutLayer / 并发闸 / CatchPanic | 代码 | 小 | 共享 tower-http feature |
| db collection 配额 + 容量 spike | 代码 + 验证 | 中 | 关系扩展性硬门槛 |
| 凭证轮换（Milvus + GitLab token） | ops | 小 | ops 动作 |
| 控制面网络隔离 + server.toml.example + deploy 硬约束 | 部署拍板 + 文档 | 小 | 全员直连 :3000 还是必经网关？ |

---

## 附：三维度详细发现索引

完整推理见各维度 agent 报告（本仓 agent-transcripts）。核心条目：

**A 健壮性与可用性**：R1=#1（SQL 写宿主机）｜R2=#2（SQL OOM）｜R3=db collection 配额｜R4 无请求超时｜R5 无限流/并发闸｜R6 handler 无 CatchPanic｜R7 list_dir 无上限｜R8 read_file 整文件物化（与 R5 叠加）｜R9 outbox 单 FIFO 饿死（应修）。已做对：上游 client 全有超时+重试、worker catch_unwind+有界并发、写入/检索资源上限、优雅关闭。

**B 数据正确性与可靠性 + 测试/CI**：核心数据路径无 blocker（outbox/worker/reconciler/Milvus 幂等/同事务入队/checksum 边界均正确）；唯一 blocker = CI 不跑测试（=#3）+ 2 个 apps 测试漏挂 `#[ignore]`；S-3 单 FIFO outbox 仍 open；collection insert→upsert（可延后）；新发现 FUSE writeback `base_rev` 丢写（opt-in，应修）。

**C 可观测性与运维部署**：M1 审计追踪全缺（=二.审计日志）｜M2 apps 鉴权 fail-open（=三）｜M3 凭证未轮换（=三）｜S1 systemd 未加固 unit（=#4）｜S2 INFO 无 per-request 日志/无 request-id｜S3 rustls-webpki patch｜S4 缺 server.toml.example + 部署硬约束。已做对：健康探针分层（/healthz vs /v1/ready）、优雅关闭、metrics_token 常量时间比对 + 404 隐藏、基础设施指标覆盖。

---

> **决策（待 Joe 拍板）**：本文档为后续动手基线。最高 ROI 批次为 #1 + #2 + #3补ignore（低风险小改）。控制面网络形态（全员直连 :3000 vs 必经网关）需先定，决定第三节是否升级为 blocker。
