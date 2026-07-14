# Veda 计划索引

> 纯索引：活跃计划在 `docs/plans/`，完成或被取代的进 `docs/archive/`。
> AGENTS.md 的工作协议指到这里；完成一个计划后归档它并更新本索引。

## 当前状态（2026-06-10）

**Alpha 已收尾，db 向量服务作为公司级服务上线推广中**（.85 生产节点已部署）。
上线前 review 与 open 项见 [`docs/reviews/review-2026-06-10-154815.md`](../reviews/review-2026-06-10-154815.md)；
零散待办在 `docs/todos.md`。

## 活跃计划（docs/plans/）

| 计划 | 状态 |
| --- | --- |
| [`veda-answer-agentic.md`](../plans/veda-answer-agentic.md) | **Stage 1 已实现 e2e 全绿**：`/v1/answer` agentic 多次召回(tool loop)+ prompt 分层;Stage 2 bot prompt 三入口贯通进行中 |
| [`veda-answer-plan.md`](../plans/veda-answer-plan.md) | ⚠️ 组装管线已被 agentic 重构取代(见上);API 契约/引用对齐仍是基础。余 DAL 真题评审(将由 qa-log 自动化) |
| [`veda-tunnel-plan.md`](../plans/veda-tunnel-plan.md) | **生产运行中**（专用机 .95）：企微长连接 + RAG 问答 + 三入口 bot 管理 + 平台 API（§18）；方向池见 `design/tunnel-directions.md` |
| [`veda-tunnel-qa-log.md`](../plans/veda-tunnel-qa-log.md) | **方案已定稿待实现**（T1 质量遥测）：问答日志 + 企微点赞点踩回流 + console 统计/bad case 清单 |
| [`db-workspace-followups.md`](../plans/db-workspace-followups.md) | 接业务方前待办：H1 Milvus GC / M1 维度校验 / A1 内存天花板（硬门槛）+ backlog 并入尾巴 |
| [`embedding-throughput-plan.md`](../plans/embedding-throughput-plan.md) | 已设计未实现，明确"先上线后优化" |
| [`outbox-dedup-refactor.md`](../plans/outbox-dedup-refactor.md) | 未实现仍有效（A-3 fencing 提交后需重对行号） |

## 方向池

- 未排期的演进方向 + 外部对标结论：[`future-directions.md`](future-directions.md)
  （tigerfs / drive9 对标；候选：operation log + 版本历史（D1）、`veda skill install`（D2））

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

- 完成或被取代的 plan：[`docs/archive/plans/`](../archive/plans/)（alpha 6 周计划、sparse/score/write_mode、平台管理面、OTLP 两篇、Java SDK）
- 旧 review 报告：[`docs/archive/reviews/`](../archive/reviews/)（留在 `docs/reviews/` 的只有 2026-04-30——C/S/W open 项锚点——和最新的 2026-06-10）
- 历史设计 / 研究稿：[`docs/archive/design/`](../archive/design/)（原始 design.md、simplify-v0、cli-skill 等，凭证/reconciler 等细节已被演进甩开，**勿当现状读**——现状见 `ARCHITECTURE.md`）
- 一次性快照：`docs/archive/` 根（alpha-tryout、production-audit、vectors-merge-backlog、handoff-*）
