# 方案：veda 向量数据面可观测增强（per-workspace/dataset + milvus）

> 状态：**已实现**（2026-06-04）。过 Codex xhigh review，按公司后端实情大幅简化。
> 前置：OTLP metrics 桥接（`observability-otlp-plan.md`）已通。本扩展新增的指标用 `metrics`
> 宏打点 → 自动进 `render()` → 被现有 exporter 搬到平台，**不动 exporter/convert**。

---

## 0. 目标

观测 **per-workspace / per-dataset** 向量性能（QPS / 延迟 / 错误率）+ **milvus 各操作维度**。

## 1. 关键事实（决定设计）

- **公司 metric 后端支持百亿级 series → 基数不是约束**（推翻本方案初稿的基数恐慌）。
- dataset 数量量级：每 workspace 几十。
- collection = workspace 一对一（`ws_{16hex(sha256(id))}_default`，`milvus.rs:34`）。
- workspace 标识 = `AuthDbWorkspace.workspace_id`（UUID），db 数据面不用 app_id。
- 唯一剩的成本：series 也存在 veda **进程内** registry，满规模（1500ws × 几十 dataset）时 veda
  内存 + `render()` 体积到几百 MB 级 —— alpha 无感，到量级再优化 / 分流 trace。**现在不加保护。**

→ 既然后端不怕基数，就**直接全维度打点**，砍掉初稿的 cap / gate / overflow / 开关 / counter 妥协 /
dataset 躲 trace。

## 2. 三层指标

端到端（用户感知）→ 向量库逻辑（不含 embedding）→ milvus 物理，逐层做差定位瓶颈。

### L1 端到端（handler，含 embedding + milvus + 组装）
- `veda_vector_request_seconds{operation, workspace_id, dataset, mode, outcome}` histogram
- 点位：`routes/vectors.rs` 4 个 handler 的 RAII `VectorReqTimer`（`?` 早退自动记 `outcome=err`）
- operation = search/upsert/query/delete；dataset = 请求值（None→`default`）；mode = search 的
  semantic/hybrid/fulltext（其余 `none`）
- **Joe 要的 per-workspace/dataset 主口径**：QPS=`rate(_count)`、P99=buckets、错误率=`outcome` 切分

### L2 向量库逻辑（store，不含 embedding）
- `veda_vector_store_op_seconds{operation, workspace_id, dataset, mode, outcome}` histogram
- 点位：`milvus.rs` 的 `VectorWorkspaceStore` impl 4 方法
- dataset/mode 只 search 有（upsert/query/delete 按 pk，填 `none`）
- 端到端 − 本层 ≈ embedding + 组装耗时

### L3 milvus 物理（post_once）
- `veda_milvus_request_seconds{operation, outcome}` histogram
- 点位：`milvus.rs` 的 `post_once`（`post` 的重试逐次经过 → 物理请求视角）
- operation = v2 path 映射**固定枚举**（`entities_search`/`entities_upsert`/…，unknown=`other`），
  **不透传 path/collection name**
- 本层延迟 = milvus 自身，定位是不是向量库慢

## 3. 指标清单

| 指标 | 类型 | labels | 层 | 点位 |
|---|---|---|---|---|
| `veda_vector_request_seconds` | histogram | operation, workspace_id, dataset, mode, outcome | 端到端 | routes/vectors.rs |
| `veda_vector_store_op_seconds` | histogram | operation, workspace_id, dataset, mode, outcome | store | milvus.rs trait impl |
| `veda_milvus_request_seconds` | histogram | operation, outcome | milvus 物理 | milvus.rs post_once |

桶：三者均 `_seconds` 后缀，复用 `obs.rs` 的 5ms–120s 桶。

## 4. Codex review 处置

**采纳（真问题）**：① 端到端在 handler 打（不在 store 层，否则漏 embedding 耗时）；② L3 operation
固定枚举、不透传 path/collection；③ search 带 `mode`（语义/全文/混合延迟不糊一起）；④ `veda-store`
不依赖 `veda-server::config`（store 层打点用 `metrics` 宏，无需 config）。

**采纳（删减）**：cap/overflow/gate、`per_workspace_*` 开关、dataset label 开关、per-workspace 粗桶
histogram、`DashSet`、exporter golden —— 百亿级后端 + dataset 几十级确认后全不需要。

**edge（暂不做）**：`error_kind` 细分（`outcome=ok/err` 现在够；要再说时加低基数枚举，不带 message）。

## 5. dataset 诉求

Joe 要按 dataset 快速区分 → dataset 直接进 L1/L2 histogram label（后端扛得住）。平台 `group by
dataset` 即可看各 dataset 的 QPS / 延迟 / 错误率。

## 6. 验证

- 单测：`milvus_operation` path→枚举 映射。
- `.161` 灰度：开 `[otlp] enabled=true` 起服务，跑 upsert/search/query/delete，平台按
  `workspace_id` / `dataset` / `mode` / `operation` group by 核对三层。

## 7. 二期 trace

per-request 细粒度（单个慢请求归因、跨 workspace 调用链）仍留 trace；metric 给聚合、trace 给个案。
