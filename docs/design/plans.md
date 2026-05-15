# Veda 实施计划

## 当前状态（2026-05-15）

**Alpha functionally complete.** 6 周 alpha 计划全部交付；最后一批收尾（S8 outbox retention + 3 个 sidecar 跟进）合入 commit `fe253c6`。

下一步不再按固定 sprint 跑，方向有三类：

1. **Dogfood**：`--write-mode=writeback` 挂载点日常使用，记问题
2. **Tryout 开放决策**：参考 [`docs/alpha-tryout.md`](../alpha-tryout.md)
3. **后续路线**：详见 [`docs/plans/alpha-plan-2026-04-29.md`](../plans/alpha-plan-2026-04-29.md) §6（多副本、ACL、PDF/OCR、tree-sitter chunking 等）

## 生效计划

[`docs/plans/alpha-plan-2026-04-29.md`](../plans/alpha-plan-2026-04-29.md) —— Phase 总览 + Week 1–6 节奏 + 后续路线。每周交付 + 已交付清单都维护在那里。

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
| 8     | 稳定化 —— 实际执行迁移到 `alpha-plan-2026-04-29.md` Week 1–6   | ✅   |

每个 Phase 的逐项任务清单已折叠到 git 历史；早期 sprint 任务表归档在 [`docs/archive/`](../archive/)。

## 归档索引

- 完成或被取代的 plan：[`docs/archive/plans/`](../archive/plans/)
- 早期 review 报告（2026-04-30 之前）：[`docs/archive/reviews/`](../archive/reviews/)
- 历史设计 / 研究稿：[`docs/archive/design/`](../archive/design/)
- Session handoff 快照：`docs/archive/handoff-*.md`
