# Veda 计划索引

> 纯索引：活跃计划在 `docs/plans/`，完成或被取代的进 `docs/archive/`。
> AGENTS.md 的工作协议指到这里；完成一个计划后归档它并更新本索引。

## 当前状态（2026-08-02）

**Alpha 已收尾，db 向量服务作为公司级服务上线**（生产 .85 / 测试 .161 .89；tunnel 生产 .95）。
上线前 review 与 open 项见 [`review-2026-06-10`](../reviews/review-2026-06-10-154815.md)；
后续两轮加固审查——[`review-2026-06-15`](../reviews/review-2026-06-15.md)（verdict BLOCKED）与
[`review-2026-06-18-final`](../reviews/review-2026-06-18-final.md)（verdict NOT launch-ready，9 条必修）——
已存档但**代码层基本未落地**，重启加固时从这两篇入手。零散待办在 `docs/todos.md`。

## 活跃计划（docs/plans/）

| 计划 | 状态 |
| --- | --- |
| [`veda-answer-agentic.md`](../plans/veda-answer-agentic.md) | **Stage 1 已实现 e2e 全绿**：`/v1/answer` agentic 多次召回(tool loop)+ prompt 分层;Stage 2 bot prompt 三入口贯通进行中 |
| [`veda-answer-plan.md`](../plans/veda-answer-plan.md) | ⚠️ 组装管线已被 agentic 重构取代(见上);API 契约/引用对齐仍是基础。余 DAL 真题评审(将由 qa-log 自动化) |
| [`veda-tunnel-plan.md`](../plans/veda-tunnel-plan.md) | **生产运行中**（专用机 .95）：企微长连接 + RAG 问答 + 三入口 bot 管理 + 平台 API（§18）；方向池见 `design/tunnel-directions.md` |
| [`veda-tunnel-qa-log.md`](../plans/veda-tunnel-qa-log.md) | **已上线**（T1 质量遥测）：问答日志 + 企微点赞点踩回流 + console 统计/bad case 清单；07-15 a72b792 平台面、07-16 1664dd0 加 `tool_trace`。可归档 |
| [`veda-answer-stream.md`](../plans/veda-answer-stream.md) | **已上线**（2026-07-14，DoD 全过，.85/.95/.89-tunnel 全部署）。可归档 |
| [`onepaas-api-alignment.md`](../plans/onepaas-api-alignment.md) | OnePaaS 接口规范对齐（分页 / 错误三件套 / 响应信封） |
| [`onepaas-veda-skill.md`](../plans/onepaas-veda-skill.md) | veda 接入 OnePaaS Skill 沙箱 / Plugin 市场：薄 Python 调 REST，不塞 CLI 不碰 FUSE |
| [`okf-knowledge-base.md`](../plans/okf-knowledge-base.md) | **不做（2026-07-23 拍板）**：现状已可承接 wiki 场景，不引入 OKF 格式层；作为设计存档保留备重启 |
| [`db-workspace-followups.md`](../plans/db-workspace-followups.md) | 接业务方前待办：H1 Milvus GC / M1 维度校验 / A1 内存天花板（硬门槛）+ backlog 并入尾巴 |
| [`embedding-throughput-plan.md`](../plans/embedding-throughput-plan.md) | **阶段 1 已实现**（07-29 最终形态：两级优先闸——交互插队/空闲时后台占满；攒批器经交叉 review 后按简化原则整体撤回，重启触发条件在 plan 内）；阶段 2 等生产数据再启 |
| [`outbox-dedup-refactor.md`](../plans/outbox-dedup-refactor.md) | **搁置**（07-29 简化审计：无性能证据不动稳定写路径；更简的中间路线备忘在 plan 头部） |

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

- 完成或被取代的 plan：[`docs/archive/plans/`](../archive/plans/)（alpha 6 周计划、sparse/score/write_mode、平台管理面、OTLP 两篇、Java SDK、coding-agent-kb——`/mcp` P0 + P1 三件套已全量，HTML/sync 等 P2 背包的触发条件在 plan 内；FUSE 三篇 fuse-plan / fuse-writeback-plan / 2026-04-24-fuse-top4-fixes；fs9-plan；pdf-word-summary-gap——08-04 当天发现修复上线+存量重刷闭环）
- 旧 review 报告：[`docs/archive/reviews/`](../archive/reviews/)。留在 `docs/reviews/` 的有 8 篇：2026-04-30（C/S/W open 项锚点）、2026-06-10（上线前 review）、2026-06-15（全员开放加固，BLOCKED，未落地）、2026-06-18 五篇（原始 + cursor + final + final-claude + final-codex，NOT launch-ready，未落地）
- 历史设计 / 研究稿：[`docs/archive/design/`](../archive/design/)（原始 design.md、simplify-v0、cli-skill 等，凭证/reconciler 等细节已被演进甩开，**勿当现状读**——现状见 `ARCHITECTURE.md`）
- 一次性快照：`docs/archive/` 根（alpha-tryout、production-audit、vectors-merge-backlog、handoff-*）
