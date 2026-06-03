# 远程服务器 E2E 测试

`crates/veda-server/tests/remote_e2e_test.rs` 是一套**纯黑盒** HTTP 端到端测试，直接打**已部署的 veda server**，验证真实的 wire 契约（serde 命名、状态码、异步索引、kind 隔离等）。它不 import 任何 veda 内部 crate，只用 `reqwest` + `serde_json`。

## 运行

```bash
# 默认打 alpha 部署
VEDA_BASE_URL=https://veda.dbpaas.dingdongxiaoqu.com \
  cargo test -p veda-server --test remote_e2e_test -- --ignored --nocapture

# 服务器较小可降并发
... -- --ignored --test-threads=4
```

- 所有用例 `#[ignore]`，普通 `cargo test` / CI 不会触发（需要网络 + 活的服务器）。
- `VEDA_BASE_URL` 不设则用上面的默认值。
- 每个 `#[tokio::test]` **完全独立**：自建账号 + workspace，跑完 best-effort 删除 workspace。失败时会残留一个无害的空账号/workspace（无删账号 API）。

## 设计要点

- **bootstrap**：`Srv::account()` 建账号拿 `vk_`；`workspace(kind)` 建 fs/db 库（名字随机，因为同账号 workspace 名唯一）；`wk(perm)` 建 `wk_` 工作区密钥。
- **认证模型**：数据面（fs 端点 + db 向量面 `/v1/vectors/*`）一律用 `wk_`——`wk_` 绑定到单个 workspace，所以 vectors 请求 body **不再带 `workspace_id`**（带了也被忽略）。控制面（datasets `/v1/workspaces/{ws}/datasets`、admin `/admin/v1/tokens`）用账号级 `vk_`，workspace 在 path 里；scoped `vk_` 的 `allowed_workspaces` 由 `load_db_workspace` 在控制面强制（越权 → 403）。JWT/账号 token 解析 workspace 的旧机制已删除。
- **最终一致**：`/v1/search` 与 summary 走异步 outbox→worker(Milvus/LLM)，用 `poll()` 轮询；`/v1/grep` 读 MySQL 同步可立即断言。db 向量 upsert 后用轻量轮询应对 Milvus 可见性延迟。

## 覆盖范围

| 分组 | 用例 | 触及端点 |
|---|---|---|
| 健康/元信息 | `health_and_meta_endpoints` | `/healthz` `/v1/ready` `/capabilities` `/install.sh` `/v1/metrics` |
| 账号/认证 | `account_create_login_and_duplicate` `anonymous_onboard_claim_login` `workspace_jwt_endpoint_removed` `auth_missing_or_garbage_rejected` | accounts(create/anonymous/claim/login)、JWT 端点已移除(404)、401 路径 |
| Workspace 管理 | `workspace_create_list_paginate_delete` `workspace_duplicate_name_rejected` | create/list(分页)/delete/keys、重名 409 |
| FS 数据面 | `fs_file_put_get_stat_head_delete` `fs_mkdir_copy_rename` `fs_conditional_writes` `fs_append_bumps_revision` `fs_partial_reads_lines_and_range` `fs_root_cannot_be_deleted` `fs_readonly_key_enforced` `fs_grep_variants` `fs_events_stream_and_cursor` | fs CRUD/stat/head、mkdir/copy/rename、If-Match(412)/If-None-Match、append、lines/Range(206/416)、根保护、只读 403、grep、SSE(回放/410/400) |
| FS 搜索/摘要 | `fs_search_dense_sparse_and_hybrid` `fs_summaries_abstract_and_overview` | search 三信号(fulltext=BM25稀疏 / semantic=cosine稠密 / hybrid=rrf融合)、path_prefix、abstract/overview(202→200) |
| FS SQL/集合 | `fs_sql_files_table_and_udtf` `fs_collections_lifecycle` `fs_collections_raw_and_duplicate` | sql(`files` 表 + `veda_fs()` UDTF)、collections 全生命周期、raw 类型、重名 409 |
| DB 向量面 | `db_vectors_roundtrip` `db_vectors_dense_semantic_search` `db_vectors_fulltext_and_hybrid_search` `db_vectors_dedup_defaults_and_autoid` `db_vectors_filter_and_projection` `db_datasets_lifecycle` `db_vectors_validation_limits` `db_workspace_resolution_and_admin_tokens` | upsert/search(三 mode：semantic COSINE / fulltext BM25 / hybrid RRF)/query/delete、稠密语义命中、稀疏+融合命中 + `score_type` 契约、去重/默认值/自动 id、meta 过滤(eq/in/range)+投影、datasets CRUD(默认库保护)、校验上限、workspace 解析 + admin token |
| 跨 kind 隔离 | `isolation_fs_workspace_rejects_db_apis` `isolation_db_workspace_rejects_fs_apis` | fs↔db 端点互斥 `WORKSPACE_KIND_MISMATCH` |

## 向量检索能力：dense / sparse / hybrid

两类 Milvus collection 都建了稠密 `vector`(COSINE) + 稀疏 `sparse_vector`(BM25 over text)，但**对外暴露的检索能力不同**：

| 端点 | dense(cosine) | sparse(BM25) | hybrid(RRF) |
|---|---|---|---|
| fs `/v1/search` | ✓ `mode=semantic` | ✓ `mode=fulltext` | ✓ `mode=hybrid`（融合稠密+稀疏）|
| db `/v1/vectors/search` | ✓ `mode=semantic` | ✓ `mode=fulltext` | ✓ `mode=hybrid`（**默认**）|
| collections search | ✓ | ✗ | ✗ |

- **fs 面三信号全覆盖**：`fs_search_dense_sparse_and_hybrid` 用两篇主题正交文档（音乐-动物 vs 财务）做硬隔离——BM25 靠稀有词（"marshmallow"/"revenue"）只命中字面所在文档；稠密靠**零词汇重叠的语义改写**命中正确主题文档；hybrid 融合两者。score_type 分别断言 `bm25`/`cosine`/`rrf`。
- **db 面三信号已接通**：`db_vectors_dense_semantic_search` 用同义改写（"kitten napping"↔"feline dozed"）证明 `mode=semantic` 按含义命中；`db_vectors_fulltext_and_hybrid_search` 复用同样的正交文档手法，证明 `mode=fulltext`（BM25 稀有词隔离）与 `mode=hybrid`（RRF 融合，默认 mode）并断言 `score_type` = `bm25`/`rrf`。

## 运行中发现的服务端问题（待修）

测试在编写/运行过程中暴露了几个真实行为，已在对应用例里做了**容忍处理**并加注释，建议后续修复：

1. **坏 SQL 返回 500 而非 4xx**：`SELECT * FROM veda_fs`（UDTF 缺参数）走到 `INTERNAL`，把"用户输入错误"泄漏成 500。`fs_sql_files_table_and_udtf` 用 `status >= 400` 容忍，理想应是 400。
2. **重名 workspace 的错误码不一致**：重复 **db** 名 → 干净的 `409 ALREADY_EXISTS`；重复 **fs** 名 → `500 INTERNAL`。同一约束两条路径错误映射不同，`workspace_duplicate_name_rejected` 注释中标注。
3. **`/admin/*` 公网不可达**：ingress(nginx) 对 `/admin/v1/tokens` 直接 405。多半是有意（admin API 内网限定）。`db_workspace_resolution_and_admin_tokens` 探测到 405/404 即跳过 scoped-token 段，保证套件对硬化代理仍全绿。
4. ~~**db collection 的 sparse 索引未被使用**~~ —— **已修复（2026-06-02）**：`/v1/vectors/search` 已接通 `mode=fulltext`（BM25）与 `mode=hybrid`（dense+BM25 RRF，默认），sparse 索引现已被使用。见 `docs/plans/db-sparse-vector-plan.md` + `db_vectors_fulltext_and_hybrid_search`。

