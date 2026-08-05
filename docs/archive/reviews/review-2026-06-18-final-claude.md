# 上线前加固审查 · 最终综合版（2026-06-18，Claude 综合）

- **HEAD**：`469f0c7`
- **来源**：综合两份独立审查 —— [`review-2026-06-18.md`](review-2026-06-18.md)（Reviewer A，多智能体 workflow，深挖多租户隔离 + 新 apps/platform 代码）与 [`review-2026-06-18-cursor.md`](review-2026-06-18-cursor.md)（Reviewer B / Cursor，深挖健壮性·可用性 / 数据正确性 / 可观测性运维）。
- **威胁模型**：已认证但不可信内部员工 + 单节点运维健壮性。三条主线：跨租户、碰宿主机、一人拖垮全员。
- **约束**：公司内部项目，**token/鉴权校验从简**（外部网关 + 网络隔离兜底，不计「缺 in-code auth」为 blocker）；不过度工程。
- **对账已读码核准**：两份报告有冲突处，本人逐条读当前代码定案，见下「§五 交叉勘误」。

---

## 一、结论

**还不能上线。** 真正卡上线的不是丢数据——两位 reviewer 都逐模块确认**默认数据路径无 blocker**（outbox lease fencing/双路径死信/退避、worker level-triggered 幂等、reconciler grace、Milvus upsert/insert 分路径幂等、写入与 ChunkSync 同事务入队、checksum 边界均正确）。卡点是**三类工程化缺口**：

1. **`/v1/sql` 能写宿主机 / 打爆内存**（碰宿主机 + 拖垮全员，两份一致定为最高 ROI）；
2. **没有资源闸门，单人即可拖垮全员**（无超时/并发闸/配额）；
3. **CI 形同虚设**（实测干净环境 `cargo test` 直接 panic，且没有任何数据面测试在跑）。

外加 **A 独家发现的一条真·跨租户写**（key revoke 漏 scope），Cursor 因无隔离维度整片漏掉。

> **两份报告的净价值**：Cursor 纠了 A 两处事实硬错（SSE、CI ignore）+ 补了 systemd/dev_mode；A 补了 Cursor 整个漏掉的隔离面 + 网关超时 + 孤儿 collection。合起来才是完整图景——单独任一份都不够。

---

## 二、必修（上线前 blocker，去重合并）

> #1+#2 是 `engine.rs::execute` **同一处编辑**；#4/#6/#7 是分钟级；#3 切 unit 是 ops；#5 是设计门槛。严重度按本威胁模型校准。

| # | 问题 | 位置 | 严重度 | 成本 | 最小修 | 来源 |
|---|------|------|--------|------|--------|------|
| 1 | **`/v1/sql` 写/读宿主机任意文件**：裸 `ctx.sql` 走默认 planner，`COPY … TO` 以 veda uid 落盘（覆盖 ExecStart 二进制 = 重启 RCE）、`CREATE EXTERNAL TABLE LOCATION` 读任意可读文件；`read_only` 只进 `FsUdfContext` 不进 planner，只读 key 也能打 | `veda-sql/engine.rs:55,116`；入口 `routes/sql.rs:24` | 🔴 blocker | 分钟 | `ctx.sql_with_options(sql, SQLOptions::new().with_allow_ddl(false).with_allow_dml(false).with_allow_statements(false))`，**无条件**。`SELECT`+`veda_*` 写 UDF 走 Projection 不受影响。补「`COPY…TO` 返 Err」回归测试 | A+B 共识 |
| 2 | **`/v1/sql` 无内存池 / 无超时 / 全量物化**：`SessionContext::new()`=无界 pool；`df.collect()` 全量（`sql.rs:27-39` 再 2× 缓冲）；cross-join 中间结果**不受 `files` 10 万行扫描上限保护** → OOM 杀整节点 | `veda-sql/engine.rs:55,119-122` | 🟠 high | 小 | `RuntimeEnvBuilder` + `GreedyMemoryPool/FairSpillPool(256–512MB)` + `df.collect()` 包 `tokio::time::timeout(30s)`。`ResourceExhausted` 已映射 500 | A+B 共识（B 点破 cross-join 缺口） |
| 3 | **CI 实测会 panic + 不跑任何数据面测试**：`vectors_http_test.rs` 3 测试仅 1 个挂 `#[ignore]`，新增的 2 个 apps 测试（`:549 apps_mgmt_company_envelope_e2e`、`:739 apps_authz_and_workspace_name_e2e`）**漏挂 ignore** → 干净环境 `cargo test` 调 `build_test_app()` 连 MySQL/Milvus **直接 panic**；两条流水线 release-only，全部数据面 HTTP 测试 `#[ignore]` 无人跑 | `crates/veda-server/tests/vectors_http_test.rs:548,738` | 🔴 blocker | 小 | ① 给 2 个测试补 `#[ignore]`（分钟级，先恢复 `cargo test` 干净通过）② 加跑真实 MySQL/Milvus 的 `cargo test` CI job（`NO_PROXY='*' --test-threads=1`） | **B 独家**（A 误报全 ignored） |
| 4 | **🆕 跨租户吊销 key**：`revoke_workspace_key` 是 `UPDATE … WHERE id=?`（**无 workspace_id**），隔壁同源 `get_workspace_key_token` 有 `AND workspace_id=?`。`revoke_app_key` 只校验项目归属、吊销时不带 `ws_id` → 持任一项目可吊销系统内任意 `wk_` | handler `routes/apps.rs:553-554`；store `mysql.rs:3099-3106` | 🟠 high ※ | 分钟 | store 加 `workspace_id` 参数 → `WHERE id=? AND workspace_id=?`，handler 传 `&ws_id`，0 行 `NotFound`（照抄隔壁） | **A 独家**（B 无隔离维度漏掉） |
| 5 | **db-workspace/collection 无全局配额** vs Milvus ~1500 loaded-collection 上限：每个 `kind=db` workspace provision 一个常驻 collection、零计数；`vk_` 面与 apps 面都可达，`create_app_project` 收**调用方可控的 `kind`**。循环建吃光集群预算 → 全员无法建/load（全局停服） | `routes/account.rs:361-415` | 🟡 medium（关系扩展硬门槛） | 中 | provision 前 **全局** `count_active_db_collections() >= ceiling` → 429/`QuotaExceeded`（per-account 无效，apps 面每路径铸新账号）。**上线前做一次目标 Milvus loaded-collection 容量 spike** | A+B 共识 |
| 6 | **生产 systemd 用未加固 unit**：已加固版（`ProtectSystem=strict` + `ReadWritePaths=/data/veda/data` + `MemoryMax=4G`）闲置无人引用，deploy 实际装的那个把加固注释掉了 | 用 `deploy/systemd/veda-server.service:37-44` 替换 `scripts/deploy/veda-server.service:21-24`（注释态） | 🟡 medium（#1/#2 的廉价纵深） | 分钟（ops） | deploy 改用加固 unit。**注**：`ReadWritePaths=/data/veda/data` 使二进制 `/data/veda/bin` **只读 → 直接挡掉 #1 的二进制覆盖 RCE**；`MemoryMax=4G` 在 cgroup 级兜 #2 的 OOM。确认 `/etc/ddmc/env.yaml` 仍可读 | **B 独家**（A 误判「沙箱挡不住二进制」） |

### 作者亲核修正

- **#1 攻击面**：prod 现 `User=veda`（非 root，`veda-server.service:19` 实测），06-15「覆盖 /root/.ssh 秒 RCE」过时；改为覆盖 `/data/veda/bin/veda-server`（属主 veda + `Restart=always`）。切到 #6 加固 unit 后此路径被堵，但 #1 代码修复仍必须（COPY 仍可写 data 目录、CREATE EXTERNAL TABLE 仍可读其他可读文件）。
- **#3 ※**：是真跨租户写不对称（两 store 函数读码对照确认）；但实际利用需**受害者 key UUID**（list 只返回自己项目的 key id，不可枚举）→ 定性「隔离类正确性 + 防御纵深」，非开箱即用洞，修复 1 行仍归必修。

---

## 三、强烈建议（低成本，与必修同批做掉）

- **🔴 中间件三件套**（`main.rs:328-331`，当前 `Cargo.toml` 仅开 `["cors","trace"]`，一次加齐 feature）：
  - `TimeoutLayer`（**必须排除 SSE `/v1/events`** —— 见 §五勘误；裸全局 30s 会砍断每条事件流、打断 FUSE/CLI long-poll。用 per-route layer 或对 `/v1/events` 豁免）；
  - `GlobalConcurrencyLimitLayer` + `LoadShedLayer`（过载快速 503）；
  - `CatchPanicLayer`（handler panic 当前 unwind 不崩节点但 drop 连接 + metric 不收尾；DataFusion 是现实 panic 点）。
  - *勘误备注*：A 单列「ConcurrencyLimit 属过度、待实测」，Cursor 主张直接加。**本人取 Cursor**——既然 SSE+timeout 改动本就在这一层，加一个保守上限的 LoadShed 边际成本几乎为零，单节点共享下「过载快速 503」比「无界排队」明显更安全；上限值给个保守默认即可，无需先压测。
- **审计日志 / request-id**（`obs/mod.rs:40-66` 仅 metrics，`/v1/sql` 这条最危险路径连 actor 都不记，控制面 mutation 也不记）：每请求一条结构化 `info!`（actor / workspace_id / sql 摘要 / 行数 / 耗时），或至少 5xx + 慢请求 + 控制面 mutation + `/v1/sql`。**与 #1 同批最划算**（出事溯源的前提）。
- **🆕 网关 reqwest 无超时**（A 独家）：`platform.rs:128,155` `reqwest::Client::new()` 无 `.timeout()`，网关假死则控制面请求永久挂起（fail-closed 只对 error 不对 hang）。单个共享 `LazyLock<Client>` 带 `.timeout(3s)`。
- **🆕 删项目漏 drop Milvus collection**（A 独家）：`delete_workspace` 只归档 MySQL，`db` workspace 的 collection 永存、无 GC。删后若 `kind==Db` 补一行 `drop_collection(...)`（幂等，复用 rollback 路径）。**别建后台 GC**。
- **`insert_rows` 行数上限 + `list_dentries` 加 LIMIT**：collection `insert_rows`（`collection.rs:85`）仿 vectors 面加 `MAX_ROWS=500`；`list_dentries`（`mysql.rs:924`）单层 list 是唯一漏 LIMIT 的（recursive/glob/grep/SQL 都已 capped），大目录 `?list` 全量物化 —— 加 LIMIT/分页。
- **凭证轮换**（凭证卫生，与「token 从简」无关）：Milvus `rw_public` + GitLab deploy token（`scripts/loadtest/*.py` 4 个 + `install.sh:27`，06-15 起按已泄露处理至今未轮换）；`install.sh` token 经无鉴权 `GET /install.sh` served（`routes/mod.rs:29,34,166`）→ 模板化或默认 `SOURCE=github`。
- **`cargo update -p rustls-webpki`**：RUSTSEC-2026-0104，编进二进制但 veda 不做 CRL/client-cert 校验、路径不可达，patch 分钟级。

---

## 四、可延后 / 鉴权从简 / 明确不修

**鉴权（按「从简」靠部署兜，不改代码）**
- apps 控制面 `VEDA_PLATFORM_BASE` 未配时 fail-open（`platform.rs:113`），list/get/**delete/revoke** 未覆盖网关 authz（A 的 #4：`delete_app_project`/`revoke_app_key` 不收 `GatewayUser`；注 delete 仍有 `account_id` 归属校验，非「谁都能删」）；`POST /v1/accounts/anonymous` 零鉴权。
- 处置：**必经注入鉴权的网关 + 安全组限 :3000 来源**，写成部署硬约束（补 `config/server.toml.example` 全键注释 + `docs/deploy.md` 安全节）。若确定只在已鉴权网关后开放，此项不算 blocker。
- *判断*：A 把「delete/revoke 加 authorize()」列为代码必修（一致性），但**按 Joe 的 token 从简取舍，这两个应归到「网关兜」而非改代码** —— 既然其余 mutating handler 的 in-code authorize 本身也只是网关已在做的事的重复，destructive 两口同样交给网关即可，不必为代码一致性单独改。除非短期内不上网关。

**可延后**
- collection `insert`→`upsert`（重复 pk，`service/collection.rs:140`，一行）—— 自伤、有界。
- delete saga 无回滚 —— 两份均降级（drop-Milvus 先行 + 幂等 + 孤儿可 retry 自愈）。
- FUSE writeback `base_rev` 楔死静默丢写 —— opt-in 实验特性，除非要对公司开放 writeback（Cursor 新发现）。
- 分页 `(page-1)*size` u32 溢出 / `resolve_workspace_name` path 插值 / 名字无字符集校验 —— A 的 DR-3/5/6，均仅自伤或有 VARCHAR 兜底，1 行级。
- 登录/建号无限流 —— 随全局限流一起覆盖。

**明确不修（已考虑并丢弃）**
- 全局 WFQ outbox 调度器、后台归档 GC reaper —— 单节点上正是要避免的复杂度，内联 drop（§三）才是正确最小范围。
- 安全响应头（CSP/HSTS）—— 纯 API 面无浏览器受害者。
- `company_envelope` 的 `usize::MAX` 缓冲 —— 缓冲的是服务端自产响应，无放大向量，**属过度修复**。

**证伪（两份交叉确认）**
- `quinn-proto`（RUSTSEC-2026-0037 高危 DoS）—— `cargo tree -i` 证实**未编进** veda-server，幻影。
- `rsa 0.9.10` Marvin 计时（RUSTSEC-2023-0071）—— veda 是 MySQL 握手加密方、无解密 oracle，不可利用且上游无 fix。
- reconciler grace 计数器持久化（A 的 DI-5）—— **前提错误**：无 reconciler 定时器（仅按需 `POST /admin/v1/reconcile/{ws}`），prod 配 `grace_passes=0`。
- `dev_mode=true 静默启动`（06-15 旧结论）—— **已失效**：`dev_mode` 现默认 `false`，有测试 `dev_mode_defaults_off` 兜底，开启打 WARN。

---

## 五、交叉勘误（两份报告的事实冲突，已读码定案）

| 冲突点 | Reviewer A | Cursor | 读码定案 | 影响 |
|--------|-----------|--------|----------|------|
| `/v1/events` 是否 SSE | 「无 SSE 路由，TimeoutLayer 无需豁免」 | 「是 SSE，必须排除」 | **Cursor 对**：`events.rs:7` `use axum::response::sse::Sse`，`:30` 注册 `/v1/events`→`sse_events` 流式 | 裸 `TimeoutLayer(30s)` 会砍断事件流 → 必须豁免 `/v1/events`。**A 的「已验证无 SSE」是 finder agent 幻觉** |
| CI `cargo test` 干净环境 | 「全部数据面测试 `#[ignore]`」 | 「2 个新 apps 测试漏挂 ignore → 直接 panic」 | **Cursor 对**：`vectors_http_test.rs` 3 测试仅 1 ignore，`:549`/`:739` 两个 apps 测试无 ignore | 升级为 blocker #3；A 的「全 ignored」错 |
| systemd 加固 | 「沙箱挡不住 #1（二进制在 /data/veda 内）」 | 「有闲置的加固 unit，换上即廉价纵深」 | **Cursor 对**：`deploy/systemd/` 加固版 `ReadWritePaths=/data/veda/**data**`，二进制在 `/data/veda/bin` → 只读 | 切 unit 即堵 #1 二进制覆盖 + 兜 #2 OOM；列为 #6 |
| `dev_mode` 默认 | （沿 06-15）默认 true | 已默认 false | **Cursor 对**：`config.rs` 测试 `dev_mode_defaults_off` | 旧结论作废 |
| 跨租户 key revoke | `revoke_workspace_key` 漏 workspace_id（真洞） | （无隔离维度，未发现） | **A 对**：`mysql.rs:3099` `WHERE id=?` 无 scope，隔壁 `:3024` 有 | 必修 #4；Cursor 整片漏掉 |

**A 独家（Cursor 漏）**：跨租户 key revoke（#4）、platform reqwest 无超时（§三）、删项目漏 drop collection（§三）、分页溢出/path 插值（§四）。
**Cursor 独家（A 漏/纠错）**：CI panic（#3）、加固 unit（#6）、SSE 豁免（§三）、dev_mode 纠正、`list_dentries` 漏 LIMIT、cross-join 不受扫描上限保护、FUSE writeback 丢写。

---

## 六、最小上线集 & 拍板清单

**最小上线集**：#1（SQL 门控）+ #2（SQL 资源上限）+ #3（补 ignore 恢复 CI + 加测试 job）+ #4（key revoke 加 scope）+ #5（collection 配额 + 容量 spike）+ #6（换加固 unit）+ 凭证轮换 + 部署写网关硬约束。其中 #1/#2/#3补ignore/#4/#6 是分钟~小时级、低风险、可独立验证。

| 项 | 类型 | 成本 | 决策点 |
|----|------|------|--------|
| #1+#2 `engine.rs` SQLOptions + memory pool + timeout | 代码 | 分钟+小 | 同一处编辑，直接修，全场最高 ROI |
| #3 补 2 处 `#[ignore]` + 加 `cargo test` CI job | 代码+CI | 小 | 先恢复 cargo test 干净通过 |
| #4 key revoke 加 `workspace_id` scope | 代码 | 分钟 | 直接修（隔离类正确性） |
| #5 db collection 全局配额 + Milvus 容量 spike | 代码+验证 | 中 | 关系扩展硬门槛，唯一非纯代码项 |
| #6 改用加固 systemd unit | ops | 分钟 | bound #1/#2，直接换 |
| 中间件三件套（TimeoutLayer 排除 SSE / 并发闸 / CatchPanic） | 代码 | 小 | 共享 tower-http feature |
| 审计日志 + request-id | 代码 | 半天 | 与 #1 同批 |
| platform reqwest 超时 / 删项目 drop collection / insert_rows 上限 / list_dentries LIMIT | 代码 | 分钟×4 | 直接修 |
| 凭证轮换（Milvus + GitLab token） | ops | 小 | ops 动作 |
| 控制面网络隔离 + server.toml.example + deploy 硬约束 | 部署拍板+文档 | 小 | 全员直连 :3000 还是必经网关？决定 delete/revoke authz 与 anonymous 是否升级 |
