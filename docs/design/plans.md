# Veda 计划索引

> 纯索引：活跃计划在 `docs/plans/`，完成或被取代的进 `docs/archive/`。
> AGENTS.md 的工作协议指到这里；完成一个计划后归档它并更新本索引。

## 当前状态（2026-08-05）

**Alpha 已收尾，db 向量服务作为公司级服务上线**（生产 .85 / 测试 .161 .89；tunnel 生产 .95）。

加固审查状态（2026-08-05 逐条核对，修正此前「代码层基本未落地」的表述——那说的是 06-15 时点）：
[`review-2026-06-18-final`](../reviews/review-2026-06-18-final.md) 的必修 11 条实况 **7 修 4 欠**：
SQL 三件套 / revoke scope / platform authz+timeout / TimeoutLayer+CatchPanic 已随 `5d755b0`（06-18 当天）落地三台；
仍欠的分两类——真欠账只有 **CI 测试守门**；db 配额 / 删 project drop collection / insert_rows 行数上限
是 **Joe 06-18 拍板暂不做**（「初期不删数据」），重启前先确认拍板是否仍有效（细目见该文头部注记）。
[`review-2026-06-15`](../reviews/review-2026-06-15.md) 作为上一轮基线保留。零散待办在 `docs/todos.md`。

2026-08-05 结构债清理：`mysql.rs` 按 trait 边界拆为 `mysql/` 模块目录；vectors / workspace
业务逻辑下沉 `veda-core`（`VectorService` / `WorkspaceService`，路由层回归薄壳）；Milvus
转义/命名/Filter DSL 收敛到 `veda_core::milvus` 单点。

## 活跃计划（docs/plans/）

| 计划 | 状态 |
| --- | --- |
| [`veda-tunnel-plan.md`](../plans/veda-tunnel-plan.md) | **生产运行中**（专用机 .95）：企微长连接 + RAG 问答 + 三入口 bot 管理 + 平台 API（§18）；方向池见 `design/tunnel-directions.md`；尾巴=server 白名单改动未部署 |
| [`veda-answer-agentic.md`](../plans/veda-answer-agentic.md) | **Stage 1 已实现 e2e 全绿**：`/v1/answer` agentic 多次召回(tool loop)+ prompt 分层;Stage 2 bot prompt 三入口贯通进行中 |
| [`onepaas-api-alignment.md`](../plans/onepaas-api-alignment.md) | OnePaaS 接口规范对齐（分页 / 错误三件套 / 响应信封） |
| [`onepaas-veda-skill.md`](../plans/onepaas-veda-skill.md) | veda 接入 OnePaaS Skill 沙箱 / Plugin 市场：薄 Python 调 REST，不塞 CLI 不碰 FUSE |
| [`db-workspace-followups.md`](../plans/db-workspace-followups.md) | 接业务方前待办：H1 Milvus GC / M1 维度校验 / A1 内存天花板（硬门槛）+ backlog 并入尾巴 |
| [`embedding-throughput-plan.md`](../plans/embedding-throughput-plan.md) | **阶段 1 已实现**（07-29 最终形态：两级优先闸——交互插队/空闲时后台占满；攒批器经交叉 review 后按简化原则整体撤回，重启触发条件在 plan 内）；阶段 2 等生产数据再启 |

## 方向池

- 未排期的演进方向 + 外部对标结论：[`future-directions.md`](future-directions.md)
  （tigerfs / drive9 / agent-memory 赛道七家对标；候选：operation log + 版本历史（D1）、
  `veda skill install`（D2）、agent/团队记忆（D3））
- Agent / 团队记忆完整设计提案：[`agent-memory.md`](agent-memory.md)
  （架构定稿；M1 三节点 + M2a 测试环境已上线，施工图均已归档；下一步 M3 操作者身份透传）

## Phase 历史

| Phase | 内容                                                          | 状态 |
| ----- | ------------------------------------------------------------- | ---- |
| 0     | 项目脚手架（Cargo workspace + 文档骨架）                       | ✅   |
| 1     | 基础层（veda-types + veda-core trait 体系）                    | ✅   |
| 2     | 存储层（MySQL + Milvus 实现）                                  | ✅   |
| 3     | Pipeline（embedding + chunking + PDF）                         | ✅   |
| 4     | HTTP 层（Axum + Worker + Reconciler + Prometheus）             | ✅   |
| 5     | SQL 引擎（DataFusion + `veda_fs*` 系列 UDFs + `search()`）     | ✅   |
| 6     | CLI（`veda` 子命令）                                           | ✅   |
| 7     | FUSE 挂载（含 write-back / debounce / cancel-on-unlink）       | ✅   |
| 8     | 稳定化 —— 实际执行见 alpha 计划（已归档）                      | ✅   |
| 9     | 承接公司向量服务：db kind / wk_ 数据面 / 平台控制面 / OTLP / Java SDK | ✅   |

## 归档索引

- 完成或被取代的 plan：[`docs/archive/plans/`](../archive/plans/)（alpha 6 周计划、sparse/score/write_mode、平台管理面、OTLP 两篇、Java SDK、coding-agent-kb——`/mcp` P0 + P1 三件套已全量，HTML/sync 等 P2 背包的触发条件在 plan 内；FUSE 三篇 fuse-plan / fuse-writeback-plan / 2026-04-24-fuse-top4-fixes；fs9-plan；pdf-word-summary-gap——08-04 当天发现修复上线+存量重刷闭环。**2026-08-05 新增四篇**：veda-answer-stream 与 veda-tunnel-qa-log——双双上线全节点后归档；okf-knowledge-base——07-23 拍板不做，设计存档备重启；gitignore-and-workspace-layout——`veda cp` ignore 规则 + `GET /v1/layout` 随 0.1.22–0.1.25 全部上线，collation 未固定问题移交 `docs/todos.md`。**2026-08-12 新增四篇**：cli-local-config——0.1.26 已发版；doc-access-stats——已全量上线三节点含生产；outbox-dedup-refactor——搁置（无性能证据不动稳定写路径，重启条件在 plan 头部）；veda-answer-plan——被 agentic 重构取代，余 DAL 真题评审由 qa-log 自动化接管。**2026-08-13 新增**：agent-memory-m1——M1 全量上线三节点（0.1.27），§6 M2/M3 预备事实仍被 m2a 引用。**2026-08-14 新增**：agent-memory-m2a——answer 双源（记忆=第二证据源+出处一类）已上线测试环境（server `3c70b305` .161/.89 + tunnel .89，`d8a0ed0`），生产随下次发版窗口；M3=操作者身份透传）
- 旧 review 报告：[`docs/archive/reviews/`](../archive/reviews/)（含 06-18 的四篇原材料稿：原始 + cursor + final-claude + final-codex）。留在 `docs/reviews/` 的有 4 篇：2026-04-30（C/S/W open 项锚点）、2026-06-10（上线前 review）、2026-06-15（全员开放加固基线）、2026-06-18-final（必修清单权威稿，头部有 2026-08-05 的 7修4欠 核对注记）
- 历史设计 / 研究稿：[`docs/archive/design/`](../archive/design/)（原始 design.md、simplify-v0、cli-skill 等，凭证/reconciler 等细节已被演进甩开，**勿当现状读**——现状见 `ARCHITECTURE.md`）
- 一次性快照与已完结实录：`docs/archive/` 根（alpha-tryout、production-audit、vectors-merge-backlog、handoff-*、loadtest-2026-06-05 与 loadtest-prod-2026-06-11 两篇压测实录。**2026-08-05 归档**：vectors-merge-plan——Phase 9 原始设计，多章节已被演进推翻，历史 § 锚仍被代码注释引用；postmortem-2026-07-empty-abstracts——空摘要事故已全闭环；word-e2e-sop / layout-cp-e2e-sop / blob-pdf-test-sop——对应功能验收完成后的一次性 SOP）
- 持续性测试 SOP 收敛在 [`docs/testing/`](../testing/)（test-sop / manual-test-sop / platform-admin-api-sop / mcp-manual-test-sop / e2e-remote-tests + `sop-fixtures/` 语料）；部署 runbook 在 `docs/` 根（deploy.md / deploy-runbook.md / deploy-tunnel.md）
