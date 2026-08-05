# 向量写入 write_mode 设计（insert 快路径）

> 状态：✅ 已实现（`fdc42a9`，2026-06-08，方案与实现同 commit）。2026-06-10 归档。
> 未兑现尾巴：Java SDK / Python 示例的 write_mode 说明（已记 todos.md）。
> 背景数据：`docs/archive/loadtest-2026-06-05.md`。

## 背景

压测在 .161 同机房裸压 Milvus 定位到：**Milvus upsert 比 insert 慢 3 倍**，多出的 ~400ms 是 upsert 语义的「查重 + delete-by-pk」开销（insert ~200ms 固定 + 0.7ms/条；upsert ~600ms）。insert 写吞吐是 upsert 的 2.4 倍（batch50：1921 vs 808 条/s）。

veda 当前 `upsert_vector_records` 把**所有**写入统一走 Milvus `/entities/upsert`，让每一次写入——包括"插入全新数据"——都付这 400ms 税。

## 决策：方案 1 + 4 组合

| | 内容 |
|---|---|
| **方案 1** | id-less 记录（服务端生成 UUID）**无条件走 Milvus insert**。UUID 必唯一，查重纯属浪费——零语义损失。 |
| **方案 4** | 写请求加 `write_mode: "upsert"(默认) \| "insert"`。默认 upsert（幂等安全，向量库该有的安全网）；写密集且保证 id 唯一的业务显式 `insert` 取 3 倍速。 |

**原则**：默认安全（重导/重试不产重复），按需提速（暴露快路径），不把唯一性责任默默推给所有调用方。

## API 契约

`POST /v1/vectors/upsert` 请求体新增可选字段 `write_mode`：

```jsonc
{
  "records": [...],
  "write_mode": "upsert"   // 可选，默认 "upsert"
}
```

四种组合语义：

| `write_mode` | `id` | 行为 | 幂等 |
|---|---|---|---|
| `upsert`（默认） | 提供 | insert-or-replace by `(workspace,dataset,id)`，现有语义 | ✅ |
| `upsert`（默认） | 省略 | 服务端 UUID → 内部走 **insert** 快路径（方案 1） | ❌（每次新 UUID） |
| `insert` | 提供 | 直接 Milvus insert，跳过查重；**调用方保证 id 唯一** | ❌ |
| `insert` | 省略 | 服务端 UUID → insert | ❌ |

**⚠️ `write_mode=insert` 的代价**：Milvus insert 不检查 pk 唯一性，重复 pk 是 **Milvus 未定义行为**（已查 Milvus 2.6 源码 + 官方文档 + 实测交叉确认）：

- 物理上为同一 pk 累积多行（`rowCount` 虚高）。⚠️ 纯 insert 的重复行**没有 tombstone**，Milvus compaction **不自动清理**——要后续对该 pk 做 delete/upsert 打了 tombstone，compaction 才可能回收旧行，所以 insert 造成的膨胀是**持久**的。
- `query` / `search` 返回**哪一条 unknown**（官方原文："which data copy will return when queried remains an unknown behavior"），且随 segment flush / compaction 时机变化——线上不可依赖、难排查。

因此 **`write_mode=insert` 契约：调用方必须保证 pk 唯一**（纯新增 / 不可复用 UUID / 应用层强约束）。任何可能产生重复 pk 的场景（**重试、CDC replay、覆盖写**）**必须用默认 upsert**——只有 upsert 有确定的「最新覆盖」语义（insert 新 + delete 旧 tombstone，compaction 收敛为一条）。

向后兼容：`write_mode` 缺省即 upsert，现有调用零影响。

## 实现

1. **store 层**（`crates/veda-store/src/milvus.rs`）：新增 `insert_vector_records`，打 `/v2/vectordb/entities/insert`（对照现有 `upsert_vector_records` 打 `/upsert`）。`VectorWorkspaceStore` trait 加对应方法。
2. **handler**（`crates/veda-server/src/routes/vectors.rs` `upsert_vectors`）：解析 `write_mode`，按规则分流——
   - `write_mode=insert`：整批走 `insert_vector_records`。
   - `write_mode=upsert`（默认）：有 id 的子批走 `upsert_vector_records`；id-less 子批走 `insert_vector_records`（方案 1）。一次请求最多两次 Milvus 调用。
3. **DTO**（`veda-types`）：`UpsertRequest` 加 `write_mode: Option<WriteMode>`（`enum WriteMode { Upsert, Insert }`，serde 默认 Upsert）。
4. 校验/embedding/dedupe-by-id（同批 last-wins）流程不变；分流发生在向 Milvus 提交那一步。

## 重试与原子性契约

**非幂等写不自动重试（Q6，最严格）**：`id-less` 和 `write_mode=insert` 都非幂等——重试会造重复 + 孤儿（Milvus 写成功但响应丢失时，客户端重试又写一条新 UUID，且拿不到第一条 id）。钉死契约：

- 服务端 + 官方 SDK（Java）对 id-less / insert 写**一律不自动重试**（SDK 已实现 id-less 不重试，此处升为正式契约）。
- 部署侧确认网关 / service mesh 不对 `/v1/vectors/*` 的 POST 自动重试。
- 文档引导业务方：**要安全重试，必须自带稳定 id**（内容哈希 / UUIDv7），化为幂等 upsert。

**混合批非原子（Q1，决议 C）**：默认 upsert 模式下，一个请求若同时含「带 id」和「id-less」记录，会拆成两次 Milvus 调用（upsert 子批 + insert 子批），**无 partial-success 原子性**——insert 子批先成功、upsert 子批失败时，整体返回失败但 id-less 已落库，重试会重生 UUID 造重复。v0 **不改分流逻辑**，以文档约束：

- 文档注明「混合批非原子；部分失败后若要重试，请整批改用显式 id」。
- `write_mode=insert` 整批、或全带 id、或全 id-less 的请求都是单次 Milvus 调用，不受此影响。

## 性能预期

基于压测：`write_mode=insert` 或 id-less 路径单请求 ~3 倍速、写吞吐 ~2.4 倍。读路径、search、query、delete 不受影响。

## 测试（真实 Milvus 集成，遵循 testing SOP）

- `write_mode=insert` 插入新 id：成功 + 延迟对照 upsert（应显著低）。
- `write_mode=insert` 唯一 id：写入成功、queryable、deletable（定义良好路径）。重复 pk 是 Milvus 未定义行为，**不测返回内容**（契约要求唯一）。
- id-less + 默认 upsert：验证内部走 insert 路径（性能 + 结果正确）。
- 默认 upsert + 显式 id：幂等回归（重放替换 in-place，不产重复）。
- 混合批（部分 id-less 部分有 id，默认 upsert）：两条记录分别落对路径。

## 落地步骤

1. `veda-types` 加 `WriteMode` enum + `UpsertRequest.write_mode`。
2. `milvus.rs` 加 `insert_vector_records` + trait 方法。
3. `vectors.rs` handler 分流逻辑。
4. 真实集成测试（上）。
5. 同步文档：`docs/api/vectors.md`、`docs/api/db-workspace-api.md`、Java SDK / Python 示例的写入说明。
