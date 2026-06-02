# 方案：db `/v1/vectors/search` 服务端按分过滤（`min_score`）

> 状态：草稿，待 Codex review + Joe 确认。
> 关联：`docs/plans/db-sparse-vector-plan.md`（mode/score_type 已落地）；本方案补"服务端按分过滤"。

## 1. 背景与现状

- 现在 db 向量检索只有 `top_k`，**没有服务端按分过滤**；"够不够相关"全靠调用侧自己判断。
- 三种 mode 的分数量纲**不可比**：`cosine ~[0,1]`（真相关度，有模型基线）/ `bm25 ~[0,30+]`（query 内可比）/ `rrf ~[0,0.033]`（**排名函数，不是相关度**）。
- 所以不能给三种 mode 套同一个分数阈值——设计必须顺着这个约束走。

## 2. 关键决策：hybrid 不做 score 过滤（已与 Joe 确认）

score 过滤就两个真实用途，hybrid 两个都不靠它解决：
1. **砍掉不相关的尾巴（数量/质量门槛）** → 用 `top_k` 即可（RAG/检索主流就是取 top_k）。
2. **拿 score 当置信度做下游决策** → RRF 是排名不是校准相关度，本就给不了；要置信度得用 cosine 或 reranker。

而"要 hybrid 的召回 + 相关度门槛"这种少数场景，直接 **`mode=semantic` + `min_score`** 就满足。**没有"非得在 hybrid 上按分过滤"的场景。**

结论：**`min_score` 只在 `semantic`/`fulltext` 生效；`hybrid`（含默认）拒绝。** 不做 autocut，不在 hybrid 上硬套阈值。契约：**hybrid = "最相关前 N，按 top_k 取"；要相关度门槛就切 semantic。**

## 3. 设计

### 3.1 请求
`VectorSearchRequest`（`api.rs`）加 `pub min_score: Option<f32>`。

### 3.2 行为
- `mode ∈ {semantic, fulltext}` + `min_score=Some(x)` → 结果里丢掉 `score < x` 的命中。
- `mode = hybrid`（**含省略 mode 走默认 hybrid**）+ `min_score=Some(_)` → **`400 INVALID_INPUT`**，message 教育：`min_score 仅支持 mode=semantic/fulltext；hybrid 按 RRF 排名（非相关度），请用 top_k，或显式 mode=semantic 做相关度门槛`。
- `min_score` 非有限值（NaN/inf）→ `400 INVALID_INPUT`。
- **不按 mode 限制取值范围**：cosine 与 bm25 量纲不同，硬 clamp 会错；`min_score>1`（cosine 下返回空）是调用方的合法选择，不报错。

### 3.3 语义
在 `top_k` 结果集上 **post-filter**：先取 top_k，再丢掉低于阈值的 → **可能返回 < top_k（甚至空）**。要更多过线结果就调大 `top_k`。文档写清这点。

### 3.4 实现
全部在 **handler**（`routes/vectors.rs`），store 不动：
1. 解析 mode 后、构造 `VectorSearchQuery` 前：若 `min_score.is_some()` 且 `mode==Hybrid` → 400；若 `min_score` 非有限 → 400。
2. store 返回 hits 后：`if let Some(ms) = req.min_score { hits.retain(|h| h.score >= ms); }`。
   - 比较方向已确认：三个分支的 `score` 都取自 Milvus 响应 `distance` 字段，且**越大越相关**（COSINE 相似度 / BM25 / RRF 均 higher=better，结果降序返回），故 `score >= ms`（保留高分）正确，无需按 mode 反向。
- **不下推 Milvus range search**（`radius`/`range_filter`）：`top_k ≤ 100`，post-filter 的浪费可忽略；原生下推留 v1，且那是"返回 top_k 个过线结果"的不同语义，需要时再切。

## 4. 明确不做（记 backlog，非本次）

- **autocut（相对断崖截断）/ rerank（cross-encoder 可解释分）**：hybrid 的相关度门槛若将来出现**具体业务需求**再做。理由见 §2——目前没有非它不可的场景，且 autocut 的 gap 启发式对 RRF 的紧密分布需要真实数据调参，现在拍常数风险大。

## 5. 文档

- `docs/api/vectors.md` + `docs/api/db-workspace-api.md`：search 端点加 `min_score` 字段；说明仅 semantic/fulltext 生效、hybrid 传入即 400、post-filter 语义（可能 < top_k）、cosine 阈值需按模型校准（给"先 embed 几对无关样本看基线"recipe）。
- `CHANGELOG`：记 `min_score`（additive 请求字段）。

## 6. 测试（real Milvus，禁 mock）

- **in-process** `crates/veda-server/tests/vectors_http_test.rs` 加 `sub_min_score`：
  - upsert 一条强相关 + 一条弱相关文档；`semantic` + 高 `min_score` → 弱相关被过滤（命中变少/为空）；低 `min_score` → 不过滤。
  - `fulltext` + `min_score` → 生效（按 bm25 分裁）。
  - `hybrid` + `min_score` → **400**。
  - **省略 mode** + `min_score` → **400**（证明默认 hybrid 也拒绝）。
- **black-box** `crates/veda-server/tests/remote_e2e_test.rs`：在 db 检索测试里加一条 `semantic` + `min_score` 断言（高阈值过滤掉弱相关、score 都 ≥ 阈值）。

## 7. DoD（已完成 2026-06-02，真实 Milvus）

- [x] `min_score` 在 semantic/fulltext 真实生效（survivors 的 score 都 ≥ 阈值）。
- [x] hybrid（含默认）+ min_score 返 400（`INVALID_INPUT`），message 清晰。
- [x] post-filter 语义文档化（可能 < top_k）。
- [x] 两层测试通过：in-process `vectors_http_test::sub_min_score`（含默认 mode 400）+ black-box `remote_e2e::db_vectors_min_score_filter`。
- [x] vectors.md + db-workspace-api.md + CHANGELOG 更新（含 cosine 校准说明）。

## 8. 决策点（开工前确认）

- **`min_score` + 默认 mode（hybrid）→ 400** 意味着：调用方**不显式写 `mode=semantic`/`fulltext` 就不能用 `min_score`**。这是有意的"逼一次有意识选择"，但属对外行为，请确认可接受（备选：默认 mode 下传 min_score 不报错而是忽略——不推荐，静默忽略用户意图更糟）。
