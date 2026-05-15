# Chunk 存储迁移调研：MySQL → MinIO

## 背景

veda 当前把文件内容直接存在 MySQL：

- 小文件：`veda_file_contents.content` (LONGTEXT)
- 大文件：切成 256KB chunk 存在 `veda_file_chunks.content` (MEDIUMTEXT)

在 50MB 文件上限下短期能跑，但长期不可持续：

- MySQL buffer pool / InnoDB redo log 被 blob 数据压榨
- 备份 / 主从复制的数据量主要来自 chunk 表
- 无法做 byte-range read、无法出 presigned URL、无法对接 CDN
- 一旦放开 50MB 上限（视频、音频、PDF），方案直接崩

本文调研把 chunk 外迁到对象存储的候选方案，给出最终方向与实现蓝图。

---

## 当前现状

| 维度 | 现状 | 位置 |
|------|------|------|
| Chunk 存储 | `veda_file_contents` (LONGTEXT) + `veda_file_chunks` (MEDIUMTEXT) | `crates/veda-store/src/mysql.rs:376-400` |
| 切分粒度 | 固定 256KB，按换行回溯 | `crates/veda-core/src/service/fs.rs:12, 938-996` |
| 内容寻址 | **文件级** SHA256（非 chunk 级），按 workspace 查重 | `crates/veda-core/src/checksum.rs:4`, `mysql.rs:732` |
| Dedup | 同 workspace 内：copy = `ref_count++` | `fs.rs:549-610` |
| 事务耦合 | chunk 写入与 dentry/file-record 写入同一 MySQL tx | `fs.rs:47-138` |
| 文件上限 | 50MB | `fs.rs:13` |
| GC | `ref_count == 0` 同事务删 chunk；无孤儿清理 | `fs.rs:484-490` |
| 抽象层 | **无独立 `ChunkStore` trait**——内容方法嵌在 `MetadataStore`/`MetadataTx` | `crates/veda-core/src/store.rs:7-121` |
| 加密 at rest | 无 | — |
| 观测 | 无 chunk IO metrics | — |

---

## 必须守住的不变式

1. **Dentry + content 的原子性**——当前是单 MySQL tx；新方案必须给出等价的 crash-safe 语义
2. **Dedup 行为**（文件级 SHA256 + ref_count + COW on `ref_count > 1`）
3. **Workspace 隔离**——不同 workspace 的存储不能彼此可见
4. **同步读语义**——`read_file_content` 当前返回完整 `String`
5. **Partial read（按行范围）**——`get_file_chunks(start_line, end_line)` 靠 `idx_line_lookup` 做行范围查询，这是为 chat/context 场景优化的能力

---

## 方案对比

### A. S3 兼容对象存储（MinIO / R2 / AWS S3 / Garage）⭐ 选定方向

**模型**：chunk content 走对象存储，key 为 `ws/{workspace_id}/files/{file_id}/{chunk_index}`；MySQL 只留 metadata（file_id、checksum、大小、chunk 索引）。

**优点**：

- 彻底切掉 MySQL 大 blob 负担，buffer pool / 备份 / 复制立刻瘦身
- 天然支持 byte-range read（未来流式读几乎零成本）
- Presigned URL → 客户端直连下载上传，veda-server 省带宽
- MinIO 自托管简单
- **和现有 outbox 机制无缝契合**：`ChunkSync` / `ChunkDelete` 事件已经在架构里

**代价**：

- **破坏 MySQL tx 原子性**——必须用 write-ahead + mark-and-sweep GC：
  - 写路径：先 PUT S3 → 再写 MySQL metadata。若 MySQL 回滚 → S3 孤儿 → 定期 orphan scanner 清理
  - 删路径：MySQL `ref_count==0` 发 outbox 事件 → 异步 worker 删 S3
- 小对象开销：PUT/GET 有固定成本
- 延迟：S3 PUT ~20-80ms vs MySQL ~5-10ms

**契合度**：高。改动集中在 `ChunkStore` trait + write-ahead + GC worker。

### B. JuiceFS

**模型**：在对象存储上挂 POSIX 文件系统。

**不契合的原因**：

- **和 veda 本身重叠**——veda 就是"带 SQL 查询和向量检索的文件系统"，JuiceFS 也是文件系统。架构层次搞反了
- JuiceFS 把 metadata 塞进自己的格式 → 失去在 MySQL 上跑 `files` table / `search()` / `embedding()` 的能力，veda 核心价值被吞掉
- 强迫我们跟它的 metadata engine 走（Redis/TiKV/MySQL 三选一）
- Dedup 在它的 4MB chunk 粒度，和当前 256KB 不对齐
- 企业版功能是商业 license

**结论**：JuiceFS 是 veda 的**同层替代**，不是组件。排除。

### C. NBD（Network Block Device）

**模型**：把远程磁盘暴露成块设备，上层跑 ext4 / GFS2。

**不契合的原因**：

- **抽象层次根本不对**——NBD 是裸块设备，还得挑文件系统：ext4 单 writer，共享文件系统（GFS2/OCFS2）运维地狱且云厂商基本不支持
- 无内容寻址 / 无 dedup / 无 ref_count，全要自己造
- 无 managed 云方案（EBS 是单机挂载，不算 NBD）

**结论**：适合"给 VM 一块远程磁盘"场景，不适合"多租户应用的 blob 存储"。排除。

### D. 本地磁盘 CAS（content-addressed store on filesystem）

**模型**：单节点本地按 `data/{sha256[0..2]}/{sha256}` 放文件。

**优点**：极简、本地 IO 最快、自然去重、零外部依赖。

**代价**：单节点、无横向扩展、备份依赖磁盘层。

**结论**：适合单机 MVP / 边缘部署。作为 `ChunkStore` 的第二个实现保留给单机场景。

### E. 混合方案：小文件 inline + 大文件对象存储 ⭐⭐ 最终选定

在 A 的基础上加一层 threshold：

- `size <= 256KB` → 继续走 `veda_file_contents.content`（保留单 tx 原子性）
- `size > 256KB` → 走对象存储

**理由**：保留常见小文件（config/chat/README）的强一致，大文件伸缩性问题用 S3 解掉。GitHub LFS / GitLab artifacts / Artifactory 都是这个思路。

---

## 最终方向

| 决策项 | 选择 | 理由 |
|--------|------|------|
| 后端 | **MinIO**（S3 兼容） | 自托管运维简单，协议是事实标准（日后平迁到 R2/AWS S3 零成本） |
| 文件上限 | 保持 **50MB** 不变 | 不改协议 / 不做 streaming，最小化 blast radius |
| 分界阈值 | **256KB**（= 当前 `CHUNK_SIZE`） | ≤1 chunk 的文件走 inline，多 chunk 才走对象存储；语义最直观 |
| Dedup 范围 | **按 workspace 隔离** | 多租户安全，S3 key 加 ws 前缀 |

**不选 JuiceFS**：同层竞品，不是组件。
**不选 NBD**：抽象层次错位。
**不选纯本地 CAS**：扩展性不够，但作为 `ChunkStore` 第二个实现保留。

---

## 数据模型变更

### MySQL

`veda_file_contents`：不动，继续承接 ≤ 256KB 的单块文件。

`veda_file_chunks`：

```sql
-- 旧
content MEDIUMTEXT NOT NULL

-- 新
blob_sha256 VARCHAR(64) NOT NULL,    -- chunk 内容 sha256，checksum 校验 & 孤儿审计
bytes_len INT NOT NULL                -- chunk 字节数（orphan scanner / 观测）
```

`start_line` / `chunk_index` 保留，`idx_line_lookup (file_id, start_line)` 保留。

按项目约定（无需向后兼容）直接 ALTER 删除 `content` 列；现有数据若需保留，提供一次性迁移脚本把旧 content 导进 MinIO。

### MinIO / S3

**Bucket**：`veda-chunks`（配置可改）

**Key 规则**：

```
ws/{workspace_id}/files/{file_id}/{chunk_index:08d}
```

- workspace 前缀：隔离 + 便于按 workspace 批量扫描/删除
- `file_id` 级子目录：复用 `copy_file` 的 COW 语义（新 `file_id` 天然写到新前缀）
- `chunk_index:08d` 零填充：按 key 字典序 = 文件顺序
- **不**把 sha256 当 key：`ref_count` 是文件级而非 chunk 级；用 `file_id` 前缀让 `delete_file` 变成 "delete by prefix"，显著简化

**Object metadata**（写入时设置）：

- `x-amz-meta-file-id`
- `x-amz-meta-workspace-id`
- `x-amz-meta-chunk-sha256`

便于 orphan scanner 审计时不查 MySQL 就能定位。

**Encryption**：MinIO 开 SSE-S3（bucket 级默认加密）。app 不做 envelope encryption。

---

## 写路径（`fs.rs::write_content`）

```
size <= 256KB
    └── tx.insert_file_content(file_id, content)   [原路径，单 tx 原子]

size > 256KB
    ├── split_into_chunks(content) -> Vec<FileChunk>
    ├── for each chunk:
    │     sha256 = compute(chunk.bytes)
    │     chunk_store.put(ws, file_id, chunk_index, bytes)   [先写 MinIO]
    ├── tx.insert_file_chunks(file_id, [{chunk_index, start_line, blob_sha256, bytes_len}])
    ├── tx.insert_file_record(...)
    ├── tx.upsert_dentry(...)
    └── tx.commit()
```

**崩溃/失败语义**：

- MinIO PUT 失败 → 整个 `write_content` 返错，MySQL tx 未开始或 rollback。MinIO 上可能有部分成功的 chunk，但 `file_id` 是新 UUID，无 MySQL 记录 → orphan scanner 24h 后清理
- MySQL commit 失败 → 同上，孤儿由 scanner 清理
- **关键**：永远先写 MinIO 再 commit MySQL。反过来会出现 MySQL 说"有"但 MinIO 404 的致命数据丢失

**COW（`ref_count > 1` 时）**：现有 fork 产出新 `file_id` → 新 chunks 写到 `ws/{ws}/files/{new_file_id}/...`，老前缀不动。老文件 ref_count-- 若降到 0 → 正常 GC。

---

## 读路径（`fs.rs::read_file_content` / `read_file_lines`）

```
storage_type == inline:
    tx.get_file_content(file_id)   [原路径]

storage_type == chunked:
    chunks = tx.get_file_chunks(file_id, start_line?, end_line?)   [原 SQL 索引定位]
    bytes = chunk_store.get_batch(ws, file_id, [chunk_index...])   [并发 GET MinIO]
    verify: sha256(bytes) == chunk.blob_sha256                     [防止 bitrot]
    concat(bytes) -> String
```

- 并发策略：`futures::stream::iter(...).buffer_unordered(8)` 或 `tokio::spawn` 并发 8-16 个 GET
- **Partial read**：MySQL 按 `start_line` 定位到需要的 chunk 范围的逻辑不动，只把 fetch 从 SQL 换成 S3 GET
- checksum 失败 → 返 `VedaError::DataCorruption`（新错误变体，映射 HTTP 500 + server 端 `error!` 日志）

---

## 删除 / GC（`fs.rs::delete_file` + outbox）

```
tx.decrement_ref_count(file_id)
if ref_count == 0:
    tx.delete_file_record(file_id)
    tx.delete_file_chunks(file_id)     [MySQL 索引立即删]
    tx.delete_file_content(file_id)    [inline 路径]
    tx.outbox_insert(ChunkDelete { workspace_id, file_id })
tx.commit()

-- 异步 worker:
on ChunkDelete { workspace_id, file_id }:
    chunk_store.delete_by_prefix(ws, file_id)
    outbox_mark_done(event_id)
```

- 同步删 MySQL 索引 → 用户立刻看不到文件
- 异步删 MinIO 对象 → worker 落后几分钟也无可见副作用
- worker 幂等：delete by prefix 即使对象已不存在也成功
- 复用现有 outbox + background worker（`ChunkDelete` 事件已存在）

---

## Orphan Scanner

**定位**：`crates/veda-tools/src/bin/orphan_scanner.rs` 或作为 `veda-server` 的一个 admin 路由

**逻辑**：

```
for workspace in all_workspaces:
    for object in s3.list(prefix = f"ws/{workspace.id}/files/"):
        file_id = parse_file_id(object.key)
        if object.last_modified < now - 24h:
            if not mysql.exists_file_record(workspace.id, file_id):
                s3.delete(object.key)
                metrics.orphan_deleted += 1
```

**调度**：每日 02:00（低峰）。先加 `--dry-run` flag，上线观察几天再启用实删。

**安全阀**：

- 24h 缓冲防止"正在写"的文件被误删
- 必须同时满足"S3 存在 + MySQL 不存在" → 漏删只会留垃圾，永远不会删活数据
- 发 prom counter + tracing span：`orphan.scanned` / `orphan.deleted` / `orphan.dry_run`

---

## 抽象：`ChunkStore` trait

位置：`crates/veda-core/src/store.rs`（和 `MetadataStore` 并列）

```rust
#[async_trait]
pub trait ChunkStore: Send + Sync {
    async fn put(&self, ws: &str, file_id: &str, chunk_index: u32, bytes: Bytes) -> Result<()>;
    async fn get(&self, ws: &str, file_id: &str, chunk_index: u32) -> Result<Bytes>;
    async fn get_batch(&self, ws: &str, file_id: &str, indices: &[u32]) -> Result<Vec<Bytes>>;
    async fn delete_by_prefix(&self, ws: &str, file_id: &str) -> Result<u32>;  // returns count
    async fn list_file_keys(&self, ws: &str, older_than: DateTime<Utc>) -> BoxStream<String>;
}
```

**实现**：

- `S3ChunkStore`（`crates/veda-store/src/s3.rs`）：用 `aws-sdk-s3`（支持 MinIO endpoint）
- `NullChunkStore`（in-memory HashMap，仅测试）：`crates/veda-core/tests/mock_store.rs`

**注入**：`VedaService` 构造时接一个 `Arc<dyn ChunkStore>`，与 `Arc<dyn MetadataStore>` 并列。

---

## 代码接触面

| 文件 | 改动类型 | 大致 LOC |
|------|---------|---------|
| `crates/veda-core/src/store.rs` | 新增 `ChunkStore` trait | +30 |
| `crates/veda-store/src/s3.rs` | **新文件**：`S3ChunkStore` 实现 | +300 |
| `crates/veda-store/src/lib.rs` | export | +2 |
| `crates/veda-store/Cargo.toml` | 加 `aws-sdk-s3`, `aws-smithy-runtime`, `bytes`, `futures` | — |
| `crates/veda-store/src/mysql.rs` | `veda_file_chunks` schema；insert/get 只存/返 metadata | ±60 |
| `crates/veda-core/src/service/fs.rs` | `write_content` 分发；`read_file_content` 并发 fetch；`delete_file` 发 outbox | +80 / -20 |
| `crates/veda-core/src/service/background/` | 新增 `ChunkDelete` handler | +60 |
| `crates/veda-server/src/main.rs` | 构造 `S3ChunkStore` 并注入 | +15 |
| `crates/veda-server/src/config.rs` | 新增 `[storage.chunks]` 配置段 | +30 |
| `crates/veda-types/src/errors.rs` | 新增 `DataCorruption`, `ChunkStoreError` 变体 | +5 |
| `crates/veda-tools/src/bin/orphan_scanner.rs` | **新 binary** | +150 |
| `crates/veda-core/tests/mock_store.rs` | `NullChunkStore` 实现 | +60 |
| `crates/veda-core/tests/fs_service_test.rs` | 适配新签名，加大文件读写用例 | +50 |
| `crates/veda-store/tests/s3_test.rs` | **新测试**：MinIO container + contract tests | +250 |
| `docs/ARCHITECTURE.md` | 新增 "Chunk Storage" 章节 | +40 |
| `docs/local-test-sop.md` | 补 MinIO docker-compose SOP + 迁移脚本 | +50 |

**预估总改动**：~+1100 LOC / -30 LOC（不含 Cargo.lock）

---

## 可复用的已有能力

- **Outbox / background worker**：`ChunkDelete` 事件已定义在现有 outbox 机制中，直接新增消费者
- **文件级 sha256**：`crates/veda-core/src/checksum.rs::sha256_hex`
- **Workspace 隔离模型**：`workspace_id` 已贯穿各层
- **`MetadataStore` / `MetadataTx` 抽象**：`ChunkStore` 沿用同样的 trait + `Arc<dyn>` 注入模式
- **COW 逻辑**：`fs.rs::write_content` 现有 `ref_count > 1 → 新 file_id` 分支无需改动，天然适配新 key 结构

---

## 验证策略

### 单元测试

`ChunkStore` **contract test suite**（用 `NullChunkStore` 和 `S3ChunkStore` 各跑一遍）：

- put / get 往返一致
- put 幂等
- delete_by_prefix 删除计数正确
- get_batch 顺序与 indices 一致
- list_file_keys 分页正确

### 集成测试

- `docs/local-test-sop.md` 更新：`docker-compose up minio` → `cargo test -p veda-store --test s3_test`
- 端到端：PUT 10MB 文件 → GET byte-for-byte 校验 → DELETE → 5s 后查 MinIO 无残留
- Partial read：PUT 10MB → GET lines=100..200 → 验证只 fetch 必要的 chunk（tracing span 计数）

### 崩溃一致性

手动注入：

- MinIO PUT 成功 + MySQL commit 前 panic → 验证下一轮 orphan scanner 清理
- 所有 chunk PUT 成功但最后一个 metadata insert 失败 → 验证孤儿被清理
- ChunkDelete worker 半途挂 → restart 后继续消费，S3 最终干净

### 性能 benchmark

- 1KB / 64KB / 1MB / 10MB / 50MB 五档文件各 100 次 PUT/GET
- 对照组：`NullChunkStore` + MySQL 旧路径 vs `S3ChunkStore` + MinIO
- 关键指标：p50 / p99 write/read latency、MySQL 表大小

### 观测

上线后需要的 metrics：

- `veda_chunk_put_duration_ms`（histogram, tag: backend=s3|inline）
- `veda_chunk_get_duration_ms`
- `veda_chunk_put_failures_total`
- `veda_orphan_scanned_total` / `veda_orphan_deleted_total`
- `veda_chunk_delete_queue_depth`（outbox 深度）

---

## 里程碑拆分（建议按顺序提 PR）

1. **M1**：引入 `ChunkStore` trait + `NullChunkStore`，改 `write_content` / `read_file_content` 走 trait。**只重构，不改行为**，测试全绿
2. **M2**：新增 `S3ChunkStore`，加 MinIO docker-compose + contract tests。不接入主路径
3. **M3**：schema 改动 + fs.rs 写路径分发到 `S3ChunkStore`（大文件），读路径同步改造。加 `DataCorruption` 错误路径
4. **M4**：outbox `ChunkDelete` consumer
5. **M5**：orphan scanner（先 `--dry-run`，上线观察几天再切实删）
6. **M6**：metrics + 文档（ARCHITECTURE.md + local-test-sop.md）

每个 M 独立可 review/merge；M3 之前回滚成本为 0。

---

## 已知风险 / 非目标

1. **不做 streaming / multipart**：保持 50MB 上限与整 body 接收，将来放开时再改协议
2. **不做 chunk 级跨文件 dedup**：维持文件级 SHA256 dedup，改动最小
3. **不做 client-side cache**：先看观测数据再决定
4. **schema 破坏性变更**：`veda_file_chunks.content` 列直接删除；生产数据需一次性迁移脚本（可放 M3 同 PR）
5. **MinIO 依赖**：开发/测试环境需起 MinIO container；CI 用 `testcontainers-rs` 自动拉起
