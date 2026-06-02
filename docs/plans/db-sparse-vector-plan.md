# 方案：db workspace 接通 Milvus sparse / BM25 / hybrid 检索

> 状态：草稿，待 Codex review + Joe 确认。
> 关联：`docs/plans/db-sparse-vector-handoff.md`（前人交接，已逐条核实，结论见 §1）、
> `docs/vectors-merge-plan.md`（v2 设计，hybrid/fulltext 原列为 v1）。
> 核实基准：HEAD `98fecdc`，`crates/veda-store/src/milvus.rs`（1494 行）。

---

## 1. 核实结论：handoff 哪些可信、哪些要修正

逐条对照代码后的判断（不迷信交接文档）：

| handoff 断言 | 核实结果 |
|---|---|
| db schema 已含 sparse 全套（`create_vector_collection` milvus.rs:403） | **属实**。`text`(jieba) + `sparse_vector`(SparseFloatVector) + BM25 function `bm25_text` + `SPARSE_INVERTED_INDEX`(metricType BM25) 都建了；7 个索引齐全。写入路径(milvus.rs:597)只写 `text`，sparse 由 Milvus 自动算。**db 确实是"差查询路径"的半成品。** |
| 引用的行号 | **全部准确**（403/550/579/643/688/882/919/971 逐一核对无误，handoff 是对着当前版本写的）。 |
| fs 侧 hybrid/fulltext "已在生产验证过" | **夸大，见 §2。** 实际上 fs 的 sparse/BM25 路径**没有任何集成测试真正断言过它工作**，并且实现细节与 Milvus 2.6 官方 REST 示例不一致。**这是本方案最重要的发现，直接影响"照抄 fs 模板"的前提。** |
| `mode` 默认值、`score_type` wire 变更 | 方向对，作为决策点保留（§5）。 |
| 召回深度 / fallback / dataset 隔离 / Strong 一致性 | 技术判断都对，纳入 §3。 |

关键修正：**vectors-merge-plan.md §3.4/§5 原本把 hybrid/fulltext 划为 v1**，v0 只做 semantic。但 §2.2 的 schema 从一开始就建了 sparse。所以本任务 = 兑现 v1 的查询路径，**无 schema 迁移、无数据回填**。

---

## 2. fs 现有实现 review（Joe 要的"fs 是否正确"）

代码位置：`query_fulltext`(milvus.rs:971)、`hybrid_search_remote`(milvus.rs:919)、`ann_search`(milvus.rs:882)。这些是 **Initial commit / 2026-04-16 的老代码**，早于整个 db 归并工作。fs 默认 `SearchMode::Hybrid`（types.rs:122 + routes/search.rs:34），所以**生产 `/v1/search` 默认就走 hybrid**。

### 结论：fs 的 sparse 路径"能跑但未经验证"，有 4 个真问题

**F1（严重）—— 整条 BM25 路径没有任何有效测试覆盖。**
- 唯一的 hybrid 测试（milvus_test.rs:98-106）只做 `let _ = store.search(&hy).await.expect("hybrid")`——**只验证不报 500，没断言任何命中或排序**。
- 而 `hybrid_search_remote` 失败会**静默 fallback 到 ANN**（milvus.rs:964-967，`Err => Ok(None)`）。**推论：即使 sparse/BM25 整条路彻底坏了，这个测试照样绿。**
- **Fulltext（纯 BM25）模式在 fs 集成测试里从未被调用过。** remote e2e 也没覆盖。
- 也就是说：fs 在 alpha 自用里"默认 hybrid"跑了很久，但**没人能证明 BM25 这一路真的贡献了结果，而不是每次都默默退化成纯 ANN**。

**F2（正确性存疑）—— `metricType` 放置与官方 REST 不符。**
- fs `query_fulltext`(milvus.rs:991) 把 `"metricType": "BM25"` 放在 **body 顶层**。
- Milvus 2.6.x 官方 full-text-search 的 cURL/REST 示例**根本不带 metricType**，只有 `"searchParams": { "params": {} }`，BM25 由 sparse 字段的索引推断。
- fs `hybrid_search_remote` 在每个 sub-request 里塞 `metricType`；官方 multi-vector-search REST 示例的 sub-request 也不带 metricType。
- 判断：fs 现写法**很可能是被 REST 网关静默忽略、靠索引推断侥幸工作**——"凑巧能跑"而非"正确"。配合 F1（没断言），无人验证过。**db 不要无脑照抄这个写法**（见 §3.3）。

**F3（质量）—— hybrid 两路召回过浅。** 两个 sub-request 的 `limit` 直接用最终 `top_k`（milvus.rs:933/943），RRF 融合前就把"两路都中游但综合很好"的结果截掉了。官方示例普遍 sub-request 取 `top_k*2` 以上。

**F4（质量）—— hybrid 失败 fallback 用 `content LIKE "%整条query%"`**（milvus.rs:891-901）。对中文多词查询，把整条 query 当连续子串匹配，召回≈0。这个 fallback 几乎等于"出错就返回空"。

### fs 是否要一起改？→ 决策点 D3（§5）

我的建议：**至少补一个"能证伪"的 fs 集成测试**（fulltext + hybrid 各一个，断言一个 dense 搜不到、BM25 能精确命中的专名 → 证明倒排真的在用）。这个测试是验证 F1/F2 的唯一手段，且**db 与 fs 共用同一套 Milvus BM25 引擎**——fs 测试跑通，等于给 db 的"照抄模板"兜底；跑挂，就说明发现了一个潜伏的生产 bug。F3/F4 是否顺手修，看 Joe。

---

## 3. db 实现方案（分层）

总原则：**以 Milvus 2.6.x 官方 REST 示例为准**（下方已引用），**不以未验证的 fs 代码为准**。dataset 隔离、Strong 一致性、output_fields 投影三条现有契约不回归。

### 3.1 类型层 `crates/veda-types`
- `api.rs:256 VectorSearchRequest` 增 `pub mode: Option<SearchMode>`（`#[serde(default)]`）。
- `types.rs:203 VectorSearchHit` 增 `pub score_type: String`：additive 响应字段（恒序列化），加 `#[serde(default = "...")]` 兼容反序列化老 payload；typed 客户端默认忽略未知字段，安全，但**要同步 SDK / db-workspace-api.md / CHANGELOG**。对齐 fs `SearchHit.score_type`(types.rs:457)：`"cosine"`∈[0,1] / `"bm25"`∈~[0,30] / `"rrf"`∈~[0,0.033]，量纲不可比，必须透传。

### 3.2 trait 层 `crates/veda-core/src/store.rs:660`
用一个 `VectorQuery` enum 按 mode 携带恰好的数据，编译期消除非法组合（fulltext 不该带 vector、semantic/hybrid 必带）：
```rust
pub enum VectorQuery<'a> {
    Semantic { vector: &'a [f32] },                // score_type = "cosine"
    Fulltext { text: &'a str },                    // score_type = "bm25"
    Hybrid   { vector: &'a [f32], text: &'a str }, // score_type = "rrf"
}
```
`VectorWorkspaceStore::search_vectors` 把原 `query_vector: &[f32]` 换成 `query: VectorQuery<'_>`，其余参数(workspace_id/dataset/top_k/extra_filter/output_fields)不变——仍是 6 参，但 mode+数据合成一个良类型参数。更新 doc 注释（现注释写死 "ranked by COSINE"，不再成立）。

### 3.3 Milvus 实现 `crates/veda-store/src/milvus.rs`
`search_vector_collection` 内 `match query`（复用 `build_dataset_active_filter` / `vector_output_fields` / `row_to_vector_record_hit` + score 映射 + extra_filter AND-merge）：

- **Semantic**：现有逻辑不动，`score_type="cosine"`。
- **Fulltext**：`annsField="sparse_vector"`，`data=[query]`，filter = base(dataset+active) AND extra_filter，`outputFields`=`vector_output_fields`，`searchParams:{ "params":{} }`（**照官方，不放顶层 metricType**），`consistencyLevel:"Strong"`，`score_type="bm25"`。
  ```json
  // Milvus 2.6.x 官方 full-text-search REST 示例（已查证）
  { "collectionName":"...", "data":["query 原串"], "annsField":"sparse",
    "limit":3, "outputFields":["text"], "searchParams":{ "params":{} } }
  ```
- **Hybrid**：`POST /v2/vectordb/entities/hybrid_search`，`search:[dense, sparse]` 两路：
  - dense：`{ data:[query_vector], annsField:"vector", filter:base AND extra, limit:fetch }`
  - sparse：`{ data:[query], annsField:"sparse_vector", filter:base AND extra, limit:fetch }`
  - **两路 sub-request 各自都带 base(dataset+active) AND extra_filter**——漏一路会跨 dataset 串数据。
  - **sparse sub-request 绝不能塞 `metricType`**：Milvus REST 该字段只接受 `L2/IP/COSINE`，BM25 靠 sparse 索引推断（Codex 核实）。dense 路 metric 同样由 AUTOINDEX COSINE 推断，省略即可。
  - `rerank:{ strategy:"rrf", params:{ k:60 } }`，顶层 `limit:top_k`，`outputFields`=`vector_output_fields`，`consistencyLevel:"Strong"`。
  - **召回深度**：sub-request `limit=(top_k*5).min(16383)`，顶层 rerank 截到 `top_k`（修 F3，db 一开始做对）。
  - **失败不 fallback，直接向上抛错（→ 5xx）**（决策 D4）：`post()`(milvus.rs:119) 已对 429/5xx 做 3 次退避重试，能到达 hybrid 代码的错误基本是确定性的 body 形状 / 索引 / 配置 bug；此时 fallback 会**每次都静默退成 semantic**，正是 F1 那个坑。失败要响。也因此**不加 `degraded`/`mode_used` wire 字段**。
- **REST body 细节（rerank 结构、sparse sub-request 形状）属"官方 cURL 未直接示例 + 现有测试未验证"项**：Codex 确认 `rerank.strategy`/`rerank.params.k`、sub-request 带 `filter` 字段名都对，但 hybrid 里 sparse raw-text 子请求官方 REST 没直接示例。**故 §4 第一步先写 hybrid 集成测试把形状钉死，再写完整实现**（TDD），不假设 fs 写法对。

### 3.4 HTTP handler `crates/veda-server/src/routes/vectors.rs:270`
- 解析 mode：**`req.mode.unwrap_or(SearchMode::Hybrid)`，显式写默认值，不要 `unwrap_or_default()`**（Codex 提醒：默认语义不该耦合到 `SearchMode::default()`）。默认 hybrid 见 D1。
- 按 mode 构造 `VectorQuery`：**仅 Semantic/Hybrid 调 embedding**，Fulltext 跳过 embed → `VectorQuery::Fulltext{text}`。现 handler 无条件 embed（vectors.rs:304），改成 `match mode`——照 fs 已有的 `search_full`(service/search.rs:138-146) 模板（`Semantic|Hybrid => embed / Fulltext => None`）。
- 其余（top_k 校验、filter DSL、output_fields 校验、resolve_db_target）不动。

---

## 4. 测试方案（real Milvus，禁 mock —— 本方案的重点）

鉴于 §2 揭示"共用引擎从未被有效验证"，测试是这次的核心交付，不是附属。

**store 层** `crates/veda-store/tests/milvus_test.rs`（db 集合上）：
- `db_fulltext_finds_lexical_only_hit`：upsert 两条，A 含专名/标识符（如 `"X9Z-7Q invoice"`）、B 是语义近似但无该 token；fulltext 查 `"X9Z-7Q"` → **断言 top1 = A**。这是"BM25 真的在用倒排、不是退化成别的"的唯一证据。
- `db_hybrid_fuses_and_returns_complex_fields`：构造一条 dense 搜不到但 BM25 命中的记录，hybrid 能召回；并**断言返回的 tags(Array)/meta(JSON) 解析正确**（验证 `entities/hybrid_search` 响应结构与 `entities/search` 一致——handoff 点 #4 未验证项）。
- `db_hybrid_surfaces_error`：注入一个会让 hybrid 失败的条件（如临时改坏 rerank 形状），断言**向上抛错、不静默退 semantic**（对齐 D4）。
- 三个分支都断言 dataset 隔离 + output_fields 投影不回归。

**HTTP 层** `crates/veda-server/tests/vectors_http_test.rs`：mode 透传、score_type 三值正确返回、fulltext 路径不触发 embed（可用 embedding mock 计数或观测）。

**fs 证伪**（D3 已定）：`milvus_test.rs` 加 fs `veda_chunks` 上的 fulltext + hybrid 断言测试（同 F1 的证伪逻辑：dense 搜不到、BM25 精确命中专名）。这一步同时验证 §3.3 要照搬的引擎行为；**先于 db 完整实现跑通**（钉死 REST 形状）。

**跑法**（既有约定）：本地 `build_router` + `config/test.toml` 指内网 Milvus/MySQL/embedding，`cargo test -- --ignored` 手动跑，**不起 docker**（CI 不跑）。

---

## 5. 决策点（已与 Joe 确认 2026-06-02）

- **D1 — db 默认 mode = `hybrid`**。无后向兼容约束（Joe 确认），按产品质量定：hybrid(dense+BM25 RRF) 对未知 query 分布召回最稳——dense 抓语义/改写，BM25 抓 dense 老漏的精确 token（ID/编号/专名/代码）；边际成本只多一次 sparse 倒排查 + RRF 合并（embed 两种 mode 都要做），且正好用上每个 db collection 已建好的 sparse/BM25。代价：默认路径硬依赖 hybrid 正确（由 §4 集成测试保证）。score_type 告诉调用方分数是 rrf。
- **D2 — `VectorSearchHit` 加 `score_type` = 是**。引入 BM25/RRF 后基本是必须的；additive + `serde(default)`，同步 SDK / db-workspace-api.md / CHANGELOG。
- **D3 — fs 范围 = db + fs 证伪集成测试**。给 fs 补能证伪 BM25 的 fulltext+hybrid 测试（验证 F1/F2、给 db 照搬兜底）；F3/F4 暂不改。**注意**：若该测试跑出 fs 确实有问题，就留下了"已知坏的代码路径"——届时要么一起修，要么打 `// FIXME` 标注，不假装没看见（Codex 提醒）。
- **D4 — hybrid 失败 = 直接抛错，不 fallback**。理由见 §3.3：transient 已被 `post()` 重试层吸收，残留错误基本是确定性 bug，fallback 只会每次静默退 semantic（= F1 坑）。因此也不加 `degraded`/`mode_used` 字段。

---

## 6. DoD（已完成 2026-06-02，真实 Milvus 2.6.14）

- [x] 三种 mode 在真实 Milvus 跑通（store + HTTP 两层；milvus_test 14 个 + vectors_http_e2e_suite 全过）。
- [x] **fulltext 证伪用例**：`db_fulltext_finds_lexical_only_hit` —— 两条共用同一 dense 向量、只有一条含专名 token，BM25 精确命中它且排除另一条，证明倒排真在用（堵住 F1）。
- [x] hybrid 召回 over-fetch `top_k*5`；`db_hybrid_fuses_and_returns_complex_fields` 断言两路都贡献（dense-near + dense-far token 都进 top-3）+ tags/meta 从 hybrid 响应正确解析；`db_hybrid_surfaces_error` 断言失败抛错不降级。
- [x] dataset 隔离、output_fields 投影、Strong 一致性不回归（全量集成测试通过）。
- [x] `score_type` 三值正确透传（HTTP `sub_search_modes` 逐 mode 断言 cosine/bm25/rrf + 默认 hybrid）。
- [x] `docs/api/vectors.md` + `docs/api/db-workspace-api.md` 加 mode/score_type/三种分数范围；CHANGELOG 记录。（`vectors-merge-backlog.md` 无对应条目，无需划掉。）
- [x] fs 证伪测试（D3）：`fs_fulltext_finds_lexical_only_hit` + `fs_hybrid_fuses_not_fallback`（用 `score_type=="rrf"` 检测静默 fallback）。**结论：fs sparse 路径实测 happy-path 正常工作**——F1 的最坏情况未成真（只是从没断言过，现已补上）；F2（顶层 metricType）实测被 Milvus 容忍、仅 cosmetic 非 bug；故**无"已知坏路径"需修/FIXME**。F3（fs hybrid 召回 limit=top_k 过浅）/ F4（fs fallback 用 LIKE）按 D3 不改，仍是 fs 侧的质量待办。

> 注：`db_hybrid_surfaces_error` 较弱（只证错误传播，不能区分"无 fallback"）；"无 fallback" 由代码结构保证（Hybrid 分支无 fallback 直接 `?`）。"fulltext 跳过 embed" 未加 HTTP spy 测试——判定为过度工程（dep+spy 测一个自证、非正确性的优化），由 handler `match` 结构保证。两点均与 Codex review 一致。

---

## 7. 工作量

类型/trait/handler 改动小（~半天）；Milvus 三分支实现 ~1 天；测试是大头（real Milvus 调试 metricType/rerank 形状）~1.5 天；文档 ~0.5 天。**合计 ~3 天**，其中测试调试有不确定性（取决于 §3.3 那些"未验证项"实测下来要不要调形状）。
