# 文档访问热度统计（doc access stats）

> 2026-08-05 起草。业务方需求：看自己 workspace 下文档被搜索命中 / 被读取的次数，只要热度趋势，不要审计级精确。
> 状态：**已实现**（同日：交叉评审 → §4 拍板 → 实现 → 单测 10 条 + 真实 MySQL 集成 2 条全绿）。实现偏差记录见 §9。

---

## 1. 需求与语义边界

业务方（fs workspace 拥有者，经 AI 工作台或原生 `wk_` 接入）想知道「哪些文档在被用、哪些是死内容」。两个指标分开：

- **search_hits**：文档出现在搜索结果里的次数（曝光量 / impression）
- **reads**：文档内容被 server 实际取出的次数（**server-side fetch**，消费量）

两者是漏斗关系：命中多、读取少 → 摘要不吸引或检索不准；读取多 → 核心资产。

**指标的精确语义**（写进对外文档，业务方对不上数时按此解释）：

1. 命中 ≠ 相关。向量检索恒返回 top-k，命中数只能看相对热度，不是精确指标。
2. agent 流量计入。一次 `ask` 内部有预检索（12 条）+ 每轮工具检索（6 条），tunnel 机器人、MCP coding agent 都在产生命中和读取。进了 LLM 上下文就算「被使用」——但这不是「人搜了多少次」。
3. **扫描面不计入**。`grep` 与 SQL 面（`veda_read()` / `veda_fs()` / SQL `search()`）是批量分析扫描——一次 grep 可扫 5 万文件、一条 SQL glob 可读 1 万文件，计入会把消费指标淹没成扫描指标。热度只统计交互与 agent 消费面：REST / MCP `read_file` / answer 工具 / FUSE / 平台文件接口。
4. **reads 是 server-side fetch**，不是「人读了几次」：FUSE 客户端缓存命中不产生请求（少计）；FUSE 写入/截断前的准备读会产生请求（多计）；大文件 Range 分段读会多计。
5. **摘要消费不计**。`/v1/abstract`、`/v1/overview`、`/v1/layout`、MCP `overview` 返回的 L0/L1 也进 LLM 上下文，但既非命中也非读取，v0 是统计盲区。
6. best-effort。进程内聚合 + 周期落库，异常时最多丢一个 flush 窗口（≤30s）的计数。

## 2. Non-goals（v0 明确不做）

- **source 维度**（人读 / agent 读区分）：业务方当前只要热度；表无需向后兼容，将来要再加列。
- **SQL `search()` UDTF 计数**：它绕过 SearchService 直连 `vector.search`（`veda-sql/src/search_table.rs:168`），且 inline 复制了 `search_full` 逻辑。按 §1.3 扫描面豁免原则，v0 不计且**豁免是语义决策而非实现漏洞**；把 UDTF 收敛到 SearchService 是独立还债项，记 `docs/todos.md`，不捆绑。
- **db-kind workspace**：裸向量面（`VectorService`）没有文档概念，不在范围。
- **citation 计数**：`/v1/answer` 的引用数才是「真被用于回答」，v1 候选。
- **摘要消费计数**（abstract/overview/layout）：见 §1.5，v1 候选。
- **console / CLI 展示**：v0 只出 API，前端按需后补。
- **dir-summary 命中 `path=None` bug 修复**：现成 bug（`summary_type` Milvus 已返回、`summary_rows_to_hits` 丢弃），两轮评审一致意见是独立排期不捆绑（客户端契约问题，与统计无关；统计侧只需稳定跳过不可归属命中，见 §3.3）。已记 todos。

## 3. 设计

### 3.1 数据模型

```sql
CREATE TABLE IF NOT EXISTS veda_doc_access_daily (
    workspace_id VARCHAR(36) NOT NULL,
    day          DATE        NOT NULL,
    dentry_id    VARCHAR(36) NOT NULL,
    search_hits  BIGINT UNSIGNED NOT NULL DEFAULT 0,
    reads        BIGINT UNSIGNED NOT NULL DEFAULT 0,
    PRIMARY KEY (workspace_id, day, dentry_id),
    KEY idx_day (day)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
```

- **按天聚合，不是每次访问一行**。fs 读路径生产压测 5.6–7.2k QPS，一次搜索去重后近 10 个命中，逐行落库写放大不可接受；OTLP 按文档打 label 基数无界，也排除。
- **聚合 key 用 `dentry_id`**：
  - 覆盖写：dentry 行不动，`dentry_id` 不变（`file_id` 在 `ref_count > 1` 时会换，`fs.rs:1738-1846`）。
  - rename / 目录 rename：只 UPDATE path，`dentry_id`、`file_id` 都不变（`mysql/tx.rs:127-177`）。
  - `path` 每 rename 断一次；`file_id` 覆盖写会断且 copy 别名下两路径计数静默合并。均不可用。
  - 删除后重建同 path = 新 `dentry_id`，历史断开——语义正确，旧文档确实没了。
  - **copy 别名归属规则**：搜索命中的身份是 `file_id`；copy 后一个 `file_id` 对应多个 dentry，命中记到**确定性选择的一个别名**头上（file→dentry 映射按 `ORDER BY path` 取首个，跨查询归属稳定）。这与现有搜索展示行为一致（`resolve_paths` 给客户端的 path 同样是单别名），计数与展示自洽。copy 在知识库场景占比极小，接受。read 侧按 path 解析 dentry，无此问题。
  - *评审分歧记录*：Codex 建议改按 `file_id` 统计「内容资产热度」回避别名问题——被拒：`file_id` 在覆盖写（`ref_count>1`）下断裂，且查询展示 path 时同样要解别名，并不更简；「路径=文档」才是业务方心智。
- **PK 顺序 `(workspace_id, day, dentry_id)`**：upsert 三列等值定位；查询按 `(workspace_id, day >= ?)` 走 PK 左前缀。`idx_day` 供 retention sweep 的 `day < cutoff` 删除（否则每轮全表扫）。
- 榜单查询是聚合查询（30 天 × 活跃文档数行参与 GROUP BY），`LIMIT` 不能早停——查询频率极低（业务方偶尔看），几万行聚合可接受；上线前生产量级 `EXPLAIN ANALYZE` 验证一次。
- **`day` 按固定 UTC 偏移取值**：`[stats] day_utc_offset_hours = 8`（默认 CST）。不依赖进程 TZ 环境（本项目在 outbox 限速上踩过 TIMESTAMP 时区坑），不引 IANA 时区库（中国无夏令时，固定偏移够）；MySQL 连接层固定 `+00:00` 只影响 TIMESTAMP，DATE 列不受影响，day 一律在 Rust 侧算好写入。record 与 query 两侧共用同一换算函数；单测注入时钟，不依赖环境时区。
- path 不冗余进表：查询时 join `veda_dentries` 拿实时 path。rename 后历史连续，删除后从榜单自然消失（inner join 过滤孤儿行）。

### 3.2 采集：`AccessRecorder`（veda-core）

```rust
pub struct AccessRecorder {
    enabled: bool,
    day_offset_hours: i8,
    buf: Mutex<HashMap<(String /*ws*/, NaiveDate, String /*dentry_id*/), Counts>>,
    meta: Arc<dyn MetadataStore>,
}
struct Counts { search_hits: u64, reads: u64 }  // saturating_add

impl AccessRecorder {
    pub fn record_search_hits(&self, ws: &str, dentry_ids: &[String]);  // 每 id +1
    pub fn record_read(&self, ws: &str, dentry_id: &str);
    pub async fn flush(&self) -> Result<usize>;
}
```

- 放 veda-core（依赖 `MetadataStore` trait，mock 可单测）；`FsService` / `SearchService` 构造函数加 `Arc<AccessRecorder>` 参数（破坏签名，无兼容负担）。disabled 时 record 直接 return。
- 临界区就是 HashMap entry += 1，std Mutex 起步。fs 搜索实际量级远低于压测上限，不预先上 dashmap；若未来争用可见（有 flush 时长指标）再换。
- **flush**（30s 周期）：`mem::take` swap 出 map，**全部行放进同一个 MySQL 事务**分批 `INSERT … ON DUPLICATE KEY UPDATE search_hits = search_hits + VALUES(search_hits), …`（行数有界——活跃文档 × 1–2 个 day——单事务无压力）。
- **失败语义：整体丢弃 + `warn!` + counter，不 merge-back**。理由：分批提交 + 全量 merge-back 会让已成功批次重试双计（两轮评审一致命中）；单事务解决原子性后，失败重试仍有 commit-结果未知的 at-least-once 边角。「只是热度」的语义档位下，失败丢一个窗口与崩溃丢一个窗口完全同档，丢弃换来零重复计数 + 省掉 merge-back/内存保险丝/对应测试三件套。MySQL 长时间不可用时服务本身早已不可用。
- **shutdown**：flush task 收现有 `shutdown_tx` watch 信号后**只退出周期循环，不做 final flush**——信号发出时 drain 窗口 / 在途请求仍在产生计数（`main.rs:417-447` 顺序），此刻 flush 必丢尾巴。final flush 由 `main` 在 `axum::serve(...).await` 返回（所有在途请求已结束）后显式调用一次并 await。worker/retention 照旧模式没这个问题，是因为它们停止认领后工作源即干涸；recorder 的工作源是 HTTP 层本身。
- **MetadataStore 新增 3 方法**：`upsert_doc_access_daily(rows)`（单事务）、`query_doc_access(ws, since, order_by, limit)`、`sweep_doc_access(cutoff)`（chunked delete，5000 行/批，走 `idx_day`）。DDL 进 `mysql/schema.rs`。

### 3.3 埋点清单

**原则：埋点只在公开方法最外层记一次；一切内部复用走不计数的 `*_inner`。** 这是两轮评审最大的共同发现——直接在五个 read 方法里各埋一行会出三种脏数据（grep 污染、preview 双计、SQL 批量放大）。

**改动一：`resolve_file` 返回值改为 `ResolvedFile { dentry: Dentry, file: FileRecord }`**（现返回 `(file_id, FileRecord)`，dentry 加载后被丢弃，`fs.rs:490-513`——方案初稿「dentry 在手零额外查询」的前半句成立、后半句需要这个签名改动兑现；全部调用方与测试随之更新）。

**改动二：`read_file` 与 `read_file_range` 拆出 `_inner` 版本**（不埋点），公开同名方法 = inner + 埋点：

| 计数的公开方法 | 覆盖的 surface |
|---|---|
| `read_file` | REST `?view=text`、MCP `read_file` 整读、answer `read_file` 工具 |
| `read_file_raw` | REST 默认下载、FUSE 全量读、平台 `file/content` |
| `read_file_preview` | 平台文件预览（内部委托改调 `read_file_range_inner`，文本预览不再双计；blob-extract 分支本就不经 range） |
| `read_file_range` | REST Range 读、**FUSE 常规读**（走 Range header，评审修正：初稿把 FUSE 归错到 raw 行）、admin 预览（运维流量，量小，接受计入） |
| `read_file_lines` | REST `?lines=`、MCP `read_file` 行读 |

**走 `_inner` 不计数的调用方**：`grep` 内部逐文件读（`fs.rs:998`，上限 5 万文件——不改则一次全库 grep 把整个 workspace 刷成已读，REST/MCP/平台三个面都暴露 grep 且 MCP 工具描述在引导 agent 用它）；SQL 面 `bounded_read_file`（`veda_read()` / `veda_fs()` glob，上限 1 万文件，`fs_udf.rs:133-150`）。worker 后台索引/摘要读走 store 直读（`worker.rs:248`），天然不经 FsService，无需处理。

**搜索命中（search_hits）**——`SearchService::search`（`service/search.rs:56`）返回前，把 hits 按文件去重后逐个 +1（同一次搜索同文件命中 3 个 chunk 算 1 次）。覆盖：REST `/v1/search`、MCP `search`、answer 预检索 + 工具轮、MCP `ask`、平台 fs search、tunnel（HTTP 回环进 REST）。两轮评审均证实此收敛成立（SQL UDTF 除外，见 §2）。

- **dentry_id 解析**：`resolve_paths` 已调 `get_dentry_paths_by_file_ids`（`mysql/metadata.rs:351-376`，本来就 SELECT `veda_dentries`），改为返回 `(file_id → (dentry_id, path))` 并加 `ORDER BY path` 保证 copy 别名归属确定（§3.1），零额外往返。
- **目录摘要命中跳过**：dir-summary 命中的 `file_id` 字段里装的实际是 `dentry_id`（`milvus.rs:967-991`；worker 对目录写的就是 dentry_id），match 不到 dentry 表 → resolve 失败。计数只对 **resolve 成功的文件命中**记账，dir 命中自然跳过。dir-summary bug 修不修都不影响此逻辑（判定条件是 resolve 成功，不是 hit 类型）。

### 3.4 查询 API

**原生面**：`GET /v1/stats/docs?days=30&limit=50&order_by=reads`（`AuthWorkspace`，fs only——提取器层面自动强制；read-only `wk_` 可查，统计是只读信息）。

- `days` 默认 30 / cap 365；`limit` 默认 50 / cap 200；`order_by` ∈ `reads` | `search_hits`。
- 响应：`{ "days": 30, "items": [ { "path": "/docs/a.md", "search_hits": 123, "reads": 45 } ] }`
- SQL：`SELECT d.path, SUM(s.search_hits), SUM(s.reads) FROM veda_doc_access_daily s JOIN veda_dentries d ON d.id = s.dentry_id AND d.workspace_id = s.workspace_id WHERE s.workspace_id = ? AND s.day >= ? GROUP BY s.dentry_id, d.path ORDER BY 2|3 DESC LIMIT ?`

**平台网关面**：`project_data.rs` 加 `GET /v1/workspace/{ws}/project/{id}/stats/docs`，`authz_and_load` 后调同一逻辑，company envelope 中间件自动改写（单 struct → 裸对象）。业务方大概率从 AI 工作台看，两面都出很便宜。

### 3.5 配置 / retention / 指标

```toml
[stats]
enabled = true              # kill switch；VEDA_STATS_ENABLED
flush_interval_secs = 30    # VEDA_STATS_FLUSH_INTERVAL_SECS，下限 5
retention_days = 365        # VEDA_STATS_RETENTION_DAYS
day_utc_offset_hours = 8    # 天边界时区，VEDA_STATS_DAY_UTC_OFFSET_HOURS
```

- **stats 的 retention sweep 挂在 flush task 内部**（每日首轮 flush 后顺带执行一次），**不进现有 `[retention]` sweep 任务**——那个任务受 `retention.enabled` 总开关控制（`main.rs:272`），关掉 events/outbox retention 不该让 stats 表无限增长。stats 开启即自带清理，配置内聚在 `[stats]` 下。
- 表增长量级：活跃文档数 / 天 / workspace，一年百万行级封顶，单表无压力；retention 兜底。
- 指标（进现有 registry，OTLP 自动带走）：`veda_doc_access_flush_seconds{outcome}`、`veda_doc_access_flushed_rows_total`、`veda_doc_access_dropped_total`、`veda_doc_access_retention_swept_total`。

## 4. 拍板记录（2026-08-05 Joe）

1. **扫描面整体豁免** ✅ 确认——热度=消费面，grep + SQL 三件套不计。
2. **source 维度** ✅ v0 不加（真正的成本是 source 要从每个路由入口传参进 service 层，埋点从两处变每入口；后补代价低，历史数据热度场景可弃）。
3. **需求方用的面** ✅ fs workspace 确认。

（初稿拍板项 dir-summary bug：两轮评审一致建议独立排期，已定不捆绑，移入 §2 + todos。）

## 5. 实现清单

| # | 改动 | 位置 |
|---|---|---|
| 1 | DDL（BIGINT + idx_day）+ MetadataStore 3 方法 | `veda-store/src/mysql/{schema,metadata}.rs`、`veda-core` trait |
| 2 | `AccessRecorder`（聚合 + 单事务 flush + 失败丢弃） | `veda-core/src/service/access_stats.rs`（新） |
| 3 | `resolve_file` → `ResolvedFile { dentry, file }` 签名改造（含全部调用方） | `veda-core/src/service/fs.rs` |
| 4 | `read_file` / `read_file_range` 拆 `_inner`；grep / SQL 面改走 inner | `veda-core/src/service/fs.rs`、`veda-sql/src/fs_udf.rs` |
| 5 | 五个公开 read 方法最外层埋点 | `veda-core/src/service/fs.rs` |
| 6 | `get_dentry_paths_by_file_ids` 返回 dentry_id + ORDER BY path | `veda-store/src/mysql/metadata.rs:361` |
| 7 | SearchService 埋点（去重 + resolve 成功才计） | `veda-core/src/service/search.rs` |
| 8 | flush task spawn（含每日 sweep）+ serve 返回后 main 显式 final flush | `veda-server/src/main.rs` |
| 9 | `GET /v1/stats/docs` + 平台面包装 | `veda-server/src/routes/`（新 stats.rs + project_data.rs） |
| 10 | `[stats]` 配置四键 | `veda-server/src/config.rs`、`config/server.toml.example` |
| 11 | 对外文档：指标语义六条（§1） | `web/public/docs/zh/reference.md` + aidoc 同步 |

## 6. 测试计划（按项目测试约定）

- **单测（mock store，注入时钟）**：Recorder 聚合（多次 record 合并 / 跨 day 分 key / 天边界偏移换算 / flush swap 清空 / 失败整体丢弃 + counter / saturating）；SearchService 计数去重（同文件多 chunk 算 1）、dir 命中跳过、copy 别名归属确定性；disabled 时零副作用。
- **单测（评审新增必测）**：`grep` 大目录后 reads 零增长；`read_file_preview` 文本文件恰好 +1（不双计）；SQL `veda_read()`/glob 后 reads 零增长。
- **集成（真实 MySQL/Milvus/embedding，`config/test.toml`）**：写文件 → search 命中 + cat 读取 → 强制 flush → `GET /v1/stats/docs` 断言计数与排序；rename 后计数延续（同 dentry_id）；删除后从榜单消失；read-only `wk_` 可查、db-kind 400；shutdown 场景（发 SIGTERM 前压着在途读请求，验证 final flush 收到尾巴计数）。
- **DoD**：集成测试全绿 + 测试节点部署后真实 workspace 验证一轮（搜索/cat/ask/grep 各来几发，榜单符合预期、grep 不污染）。

## 7. 后续候选（不承诺）

- source 维度列（human / agent / platform）
- citation 计数（answer 引用）+ 摘要消费计数（abstract/overview/layout）
- console 热度面板（tunnel 统计卡先例）
- CLI `veda stats docs`
- SQL UDTF 收敛到 SearchService（还债，捎带修 path_prefix / detail_level 缺失）
- dir-summary 命中 `path=None` 修复（独立排期，todos 有记）

## 8. 交叉评审记录（2026-08-05）

Claude subagent + Codex（xhigh）对初稿独立评审，行号级交叉验证。共同命中并已修订：grep 污染 reads（两边最高严重级）、preview 双计、shutdown final flush 时机、分批 flush merge-back 双计、sweep 缺 day 索引。单边命中并采纳：`resolve_file` 丢弃 dentry（Codex）、FUSE 归类错误 + SQL 批量放大 + 摘要盲区（Claude）、BIGINT / 时区契约 / retention 开关耦合（Codex）。**拒绝**：按 `file_id` 聚合（Codex O1，理由见 §3.1 分歧记录）、IANA 时区配置（固定偏移够）、配置挪 `[retention]`（Codex O2，内聚性优先）。初稿其余全部行号断言经双方核实无误。

## 9. 实现偏差记录（2026-08-05 实现收尾）

实现与本方案的偏离处，及原因：

1. **服务构造不破坏签名**：方案说「构造函数加参数，破坏签名」；实际 `FsService::new`/`SearchService::new` 保持原样（内部默认 disabled recorder），生产组装走新增的 `with_stats()`。原因：`new` 有 20+ 测试构造点，opt-in 构造器让测试零波及，且「默认构造即不计数」恰好就是 SQL 豁免的表达方式（见下）。代价是新增调用点默认不计数——由集成测试守住生产组装。
2. **SQL 豁免在组装层实现,veda-sql 零改动**：方案 §5.4 说改 `fs_udf.rs` 走 inner；实际是 main.rs 给 SQL engine 传一个 `FsService::new`（不计数）实例。更简，且豁免语义集中在组装处一行注释。
3. **列名 `reads` → `read_count`**：READS 是 MySQL 保留字，migrate 打真实 8.0 测试库当场 1064。API 字段名 `reads` 不变（查询层映射）。
4. **preview 的「暂不支持预览」占位响应不计 read**（方案未细化到此分支）：没有内容被取出，计入会让图片在热度榜上显得「被阅读」，与 §1.4 的 server-side fetch 定义不符。
5. **集成测试的 search_hits 经 recorder 直接注入**,不驱动 embedding worker 做真实检索：search→去重→record 链路已被单测 pin 死（mock 命中走真实 `resolve_paths`），真实 hybrid 检索已被 mcp_http_test 覆盖，集成层真正需要真实 MySQL 的是 hits/reads 共享的 upsert/聚合 SQL。完整端到端（搜索→计数）留给测试节点部署验证（§6 DoD 第二条,未做）。
6. **§6 的 shutdown 集成测试未做**：final flush 尾巴（SIGTERM → drain → serve 返回 → flush）需要真进程级测试,oneshot router 测不了；丢的又是 best-effort 计数,成本收益不成立。代码路径经两轮评审走查确认,部署验证时人工看一次日志即可。

**实现后交叉评审（第二轮,2026-08-05）**：Claude + Codex 对实现再审。verdict：Claude「可提交」/ Codex「先修 copy 排序」。已修 4 条：① copy 别名 `ORDER BY path` 在 CI collation 下大小写等值路径排序不定（Codex 唯一 MAJOR,与 todos 的 collation 未固定债呼应）→ 加 `, id` tie-break + mock 镜像 + 归属确定性单测；② `day_utc_offset_hours` 越界被 `east_opt` 静默回退 UTC → config 加载时校验 -23..=23 越界启动报错（fail-fast,parse_args 同哲学）；③ `read_file_lines` 恒真 `if out.is_ok()`（错误路径全部 `?` 早退）→ 直接 record；④ 删除断言过弱的 `default_constructor_records_nothing`（无 flush 入口,区分不了 disabled 与未 flush——Codex 指出的假绿）,改由集成测试打真 `POST /v1/sql veda_read()` 断言组装级 SQL 豁免 + additive upsert 二轮 flush 断言 + 负偏移单测。**拒绝**：为大小写别名 copy 建真库回归 fixture（edge-of-edge,注释已写明机理）;raw/range/lines 三入口逐个埋点单测（五方法埋点同构,read_file/preview 已覆盖）;并发 record+flush 时序测试（std Mutex 语义保证）;>500 行 chunk 边界与 SIGTERM 进程级测试（过度）。对外文档 UTC+8 表述补「服务端可配」。
