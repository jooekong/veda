# Veda 本地测试 SOP

## 1. 环境准备

### 1.1 依赖服务

| 服务 | 版本 | 用途 | 测试类型 |
|------|------|------|----------|
| MySQL | 8.0+ | 元数据存储 | 集成测试 |
| Milvus | 2.4+ | 向量存储 | 集成测试（可选） |
| Embedding API | OpenAI 兼容 | 文本向量化 | 集成测试（可选） |

### 1.2 配置文件

```bash
# 复制配置模板
cp config/test.toml.example config/test.toml

# 编辑配置（根据实际环境修改）
vim config/test.toml
```

**config/test.toml 示例：**

```toml
[mysql]
database_url = "mysql://user:password@localhost:3306/veda"

[milvus]
url = "http://localhost:19530"
token = ""
db = "default"

[embedding]
api_url = "https://api.openai.com/v1/embeddings"
api_key = "sk-xxx"
model = "text-embedding-3-small"
dimension = 1024
```

### 1.3 数据库初始化

```bash
# 创建测试数据库
mysql -u root -p -e "CREATE DATABASE IF NOT EXISTS veda CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci;"

# 迁移会在测试时自动执行
```

---

## 2. 测试分类

### 2.1 单元测试（无外部依赖）

无需 MySQL/Milvus，使用 mock 或纯逻辑测试。

| Crate | 测试内容 | 文件位置 |
|-------|----------|----------|
| veda-types | 类型序列化/反序列化 | `tests/types_test.rs` |
| veda-core | 路径规范化、SHA256 | `src/path.rs`, `src/checksum.rs` |
| veda-core | FsService 业务逻辑 | `tests/fs_service_test.rs`（mock store） |
| veda-pipeline | 语义分块算法 | `tests/chunking_test.rs` |
| veda-sql | DataFusion SQL 引擎 | `tests/sql_test.rs`（mock store） |
| veda-fuse | LRU 缓存、SSE 解析 | `src/cache.rs`, `src/sse.rs`（内联测试） |

### 2.2 集成测试（需 MySQL）

标记为 `#[ignore]`，需 `--ignored` 标志运行。

| Crate | 测试内容 | 文件位置 |
|-------|----------|----------|
| veda-store | MySQL CRUD、事务、任务队列 | `tests/mysql_test.rs` |
| veda-server | HTTP API 端到端 | `tests/server_test.rs` |

### 2.3 集成测试（需 Milvus）

标记为 `#[ignore]`，需 `--ignored` 标志运行。

| Crate | 测试内容 | 文件位置 |
|-------|----------|----------|
| veda-store | 向量 CRUD、搜索、动态 Collection | `tests/milvus_test.rs` |

### 2.4 集成测试（需 Embedding API）

标记为 `#[ignore]`，需 `--ignored` 标志运行。

| Crate | 测试内容 | 文件位置 |
|-------|----------|----------|
| veda-pipeline | Embedding API 调用 | `tests/embedding_test.rs` |

---

## 3. 测试执行命令

### 3.1 快速验证（仅单元测试）

```bash
# 运行所有非 ignored 测试
cargo test --workspace

# 或逐个 crate 运行（更快反馈）
cargo test -p veda-types
cargo test -p veda-core
cargo test -p veda-pipeline
cargo test -p veda-sql
```

**预期输出：**
- veda-types: 9 tests passed
- veda-core: 47+ tests passed（含 fs_service_test）
- veda-pipeline: 3 tests passed（chunking）
- veda-sql: 45+ tests passed
- veda-fuse: 7 tests passed（内联）

### 3.2 MySQL 集成测试

```bash
# 确保 MySQL 可达且 config/test.toml 配置正确
cargo test -p veda-store -- --ignored

# 或单独运行某个测试
cargo test -p veda-store mysql_migrate_and_dentry_crud -- --ignored
```

**测试列表：**
- `mysql_migrate_and_dentry_crud`
- `mysql_list_dentries_under_root_lists_workspace_entries`
- `mysql_file_and_content`
- `mysql_tx_atomic_dentry_and_file`
- `mysql_task_queue_enqueue_claim_complete`
- `mysql_checksum_lookup_and_delete_file`
- `mysql_account_crud`
- `mysql_api_key_crud`
- `mysql_workspace_crud`
- `mysql_workspace_key_crud`
- `mysql_collection_schema_crud`
- `mysql_rename_dentries_under_unicode_prefix`

### 3.3 Milvus 集成测试

```bash
# 确保 Milvus 可达
cargo test -p veda-store milvus -- --ignored --test-threads=1
```

**测试列表：**
- `milvus_init_upsert_search_delete`
- `milvus_dynamic_collection_crud`

**注意：** Milvus 删除有可见性延迟，需 `--test-threads=1` 串行运行。

### 3.4 Embedding 集成测试

```bash
# 确保 Embedding API 可达
cargo test -p veda-pipeline embedding -- --ignored
```

**测试列表：**
- `embedding_single_text`
- `embedding_batch`
- `embedding_dimension_matches_config`
- `embedding_empty_input`

### 3.5 Server 端到端测试

```bash
# 需 MySQL + Embedding API（部分测试）
cargo test -p veda-server -- --ignored
```

**测试列表：**
- `server_e2e_basic_fs_crud_and_stat` - 文件 CRUD + stat
- `server_e2e_mkdir_copy_rename_lines_and_range` - 目录操作 + 复制 + 重命名 + 行读取 + Range 读取
- `server_e2e_conditional_write_and_append` - CAS 写入 + append
- `server_e2e_auth_and_root_guards` - 认证 + 权限 + 根目录保护

### 3.6 完整集成测试（全部 ignored）

```bash
# 一键运行所有集成测试（需 MySQL + Milvus + Embedding API）
cargo test --workspace -- --ignored --test-threads=1
```

---

## 4. 测试场景清单

### 4.1 veda-core（FsService 业务逻辑）

| 测试场景 | 测试名 | 验证点 |
|----------|--------|--------|
| 基础读写 | `write_and_read` | 写入后读取内容一致 |
| 自动建目录 | `write_creates_parent_dirs` | 写入深层文件自动创建父目录 |
| 内容去重 | `dedup_same_content` | 相同内容不重复写入，返回 `content_unchanged=true` |
| 版本递增 | `overwrite_bumps_revision` | 覆盖写入时 revision +1 |
| Outbox 事件 | `overwrite_enqueues_outbox` | 覆盖写入触发 ChunkSync 事件 |
| 读不存在 | `read_nonexistent_returns_not_found` | 返回 NotFound 错误 |
| 写入目录路径 | `write_to_dir_path_fails` | 返回 AlreadyExists 错误 |
| 删除文件 | `delete_file` | 删除后读取返回 NotFound，触发 ChunkDelete 事件 |
| 删除根目录 | `delete_root_fails` | 返回 InvalidPath 错误 |
| 目录操作 | `mkdir_and_list` | mkdir 后 list_dir 返回正确条目 |
| 目录幂等 | `mkdir_idempotent` | 重复 mkdir 不报错 |
| 文件 stat | `stat_file` | 返回正确 size_bytes、revision、is_dir=false |
| 目录 stat | `stat_dir` | is_dir=true，file_id=null |
| 根目录 stat | `stat_root_virtual` | 虚拟根目录 stat 成功 |
| 复制文件 COW | `copy_file_cow` | 复制后 ref_count=2，读取内容相同 |
| 重命名文件 | `rename_file` | 旧路径消失，新路径可读 |
| 重命名到已存在 | `rename_to_existing_fails` | 返回 AlreadyExists 错误 |
| 行号读取 | `read_lines` | 按行号范围读取正确 |
| 行号边界 | `read_lines_whole_file` / `read_lines_past_eof_returns_empty` / `read_lines_clamps_end_to_eof` | 超出 EOF 返回空，EOF 边界正确截断 |
| 行号非法 | `read_lines_invalid_range_rejected` | start=0 或 start>end 返回 InvalidInput |
| 行号范围过大 | `read_lines_range_too_large_rejected` | 超过 100000 行返回 InvalidInput |
| 读取目录行号 | `read_lines_on_directory_rejected` | 返回 InvalidPath |
| Chunked 行号读取 | `read_lines_chunked_across_chunks` | 跨 chunk 读取正确 |
| 超大单行 | `read_lines_chunked_oversized_single_line` | 单行 > 256KB 时正确读取 |
| Chunk 过滤 | `read_lines_chunked_fetches_only_overlapping_chunks` | 深层行号只查询相关 chunk |
| FS 事件 | `fs_events_emitted` | Create/Delete 事件正确触发 |
| 工作空间隔离 | `workspace_isolation` | 跨 workspace 读取返回 NotFound |
| 删除目录清理 | `delete_dir_cleans_up_child_files` | 删除目录时清理子文件，触发 ChunkDelete |
| Append COW 隔离 | `append_file_cow_isolation` | Append 不会影响共享引用的文件 |
| Append 新文件 | `append_creates_new_file` | Append 不存在的路径创建新文件 |
| Append 已存在 | `append_to_existing_file` | 追加后内容正确，revision +1 |
| 文件大小限制 | `write_file_size_limit` | 超过 50MB 返回 QuotaExceeded |
| 根目录列表 | `list_dir_root` | 根目录列表正确 |
| 复制覆盖 | `copy_overwrite_decrements_old_ref_count` | 覆盖复制时旧文件 ref_count 减少 |
| Range 读取 | `read_file_range_returns_partial_content` | Range 请求返回正确字节范围 |
| If-None-Match | `if_none_match_skips_rewrite` | SHA256 匹配时跳过写入 |
| If-None-Match 跨路径 | `if_none_match_does_not_fire_on_different_path` | SHA256 匹配其他文件时不跳过 |
| 增量 Append | `incremental_append_preserves_prefix_chunks` | Append 只重写尾部 chunk，前缀 chunk 保持不变 |

### 4.2 veda-store/mysql（MySQL 存储）

| 测试场景 | 测试名 | 验证点 |
|----------|--------|--------|
| 迁移 + Dentry CRUD | `mysql_migrate_and_dentry_crud` | 迁移成功，dentry 增删查正确 |
| 根目录列表 | `mysql_list_dentries_under_root_lists_workspace_entries` | 仅返回当前 workspace 条目 |
| 文件 + 内容 | `mysql_file_and_content` | 文件记录和内容存储正确 |
| 事务原子性 | `mysql_tx_atomic_dentry_and_file` | 事务内 dentry + file 原子写入 |
| 任务队列 | `mysql_task_queue_enqueue_claim_complete` | enqueue → claim → complete 流程正确 |
| Checksum 查找 + 删除 | `mysql_checksum_lookup_and_delete_file` | 按 checksum 查找文件，删除后消失 |
| Account CRUD | `mysql_account_crud` | 账户创建、查询、邮箱查找 |
| API Key CRUD | `mysql_api_key_crud` | Key 创建、查询、撤销 |
| Workspace CRUD | `mysql_workspace_crud` | Workspace 创建、查询、删除（归档） |
| Workspace Key CRUD | `mysql_workspace_key_crud` | Workspace Key 创建、查询、撤销 |
| Collection Schema CRUD | `mysql_collection_schema_crud` | Schema 创建、查询、列表、删除 |
| Unicode 路径 rename | `mysql_rename_dentries_under_unicode_prefix` | 中文路径 rename 后 parent_path 正确 |

### 4.3 veda-store/milvus（向量存储）

| 测试场景 | 测试名 | 验证点 |
|----------|--------|--------|
| 向量 CRUD | `milvus_init_upsert_search_delete` | 初始化、upsert、search、delete 流程 |
| 搜索模式 | 同上 | Semantic + Hybrid 搜索 |
| 动态 Collection | `milvus_dynamic_collection_crud` | 创建、插入、搜索、删除 Collection |

### 4.4 veda-pipeline（Embedding + Chunking）

| 测试场景 | 测试名 | 验证点 |
|----------|--------|--------|
| Markdown 分块 | `semantic_chunk_splits_on_markdown_headings` | 按 heading 分块正确 |
| 滑动窗口 | `semantic_chunk_sliding_window_for_long_section` | 超长 section 使用滑动窗口 |
| 空输入 | `semantic_chunk_empty_input` | 返回空数组 |
| 单文本 Embedding | `embedding_single_text` | 返回正确维度向量 |
| 批量 Embedding | `embedding_batch` | 批量处理正确 |
| 维度匹配 | `embedding_dimension_matches_config` | 向量维度与配置一致 |
| 空输入 Embedding | `embedding_empty_input` | 返回空数组 |

### 4.5 veda-sql（DataFusion SQL）

| 测试场景 | 测试名 | 验证点 |
|----------|--------|--------|
| 全表扫描 | `select_star_from_files` | 返回所有文件 |
| WHERE 过滤 | `where_filter_on_files` | is_dir 过滤正确 |
| COUNT 聚合 | `count_files` | 聚合函数正确 |
| Collection 查询 | `collection_query` | 查询 Collection 表 |
| Collection WHERE | `collection_where_filter` | Collection 条件过滤 |
| 空工作空间 | `empty_workspace_returns_empty` | 返回空结果 |
| 无效 SQL | `invalid_sql_returns_error` | 返回错误 |
| 嵌套目录 | `files_with_nested_dirs` | 嵌套路径正确 |
| file_id 列 | `select_file_id_from_files` | file_id 正确返回 |
| LIKE 前缀过滤 | `files_path_like_prefix_filter` | path LIKE 过滤正确 |
| path 等值过滤 | `files_path_eq_filter` | path = 过滤正确 |
| LIMIT | `files_limit` | LIMIT 正确 |
| 完整文件记录 | `files_exposes_full_file_record` | ref_count、line_count 等字段正确 |
| 目录字段 NULL | `files_new_fields_null_for_dirs` | 目录的 ref_count 等为 NULL |
| file_id NULL for dirs | `file_id_is_null_for_dirs` | 目录 file_id 为 NULL |
| UDF veda_write/read | `udf_veda_write_and_read` | 写入后读取正确 |
| UDF veda_exists | `udf_veda_exists` | 存在性检查正确 |
| UDF veda_size/mtime | `udf_veda_size_and_mtime` | 大小和时间正确 |
| UDF veda_append | `udf_veda_append` | 追加正确 |
| UDF veda_mkdir/remove | `udf_veda_mkdir_and_remove` | 目录操作正确 |
| UDF 列参数 exists | `udf_column_arg_veda_exists` | 以列为参数调用正确 |
| UDF 列参数 read | `udf_column_arg_veda_read` | 以列为参数读取正确 |
| veda_fs 目录列表 | `veda_fs_dir_listing` | 表函数目录列表 |
| veda_fs 读取文本 | `veda_fs_read_plain_text` | 按行读取文本 |
| veda_fs 读取 CSV | `veda_fs_read_csv` | CSV 解析正确 |
| veda_fs 读取 JSONL | `veda_fs_read_jsonl` | JSONL 解析正确 |
| veda_fs glob | `veda_fs_glob` | 通配符匹配正确 |
| veda_fs_events | `veda_fs_events_basic` / `veda_fs_events_since_id` | 事件查询正确 |
| veda_storage_stats | `veda_storage_stats_basic` | 存储统计正确 |
| UDF 错误处理 | `udf_read_not_found_returns_null` / `udf_exists_not_found_returns_false` / `udf_size_not_found_returns_null` | 不存在返回 NULL/false |
| UDF remove 错误 | `udf_remove_nonexistent_errors` | 删除不存在返回错误 |
| UDF 错误信息 | `udf_error_message_is_friendly` | 错误信息友好 |
| 只读模式 | `read_only_rejects_write_udf` / `read_only_allows_read_udf` | 只读拒绝写操作 |
| embedding UDF | `embedding_returns_json_vector` / `embedding_with_column_arg` | Embedding UDF 正确 |
| search UDTF | `search_returns_results` / `search_with_mode_and_limit` / `search_empty_results` / `search_invalid_mode_errors` / `search_with_where_filter` | Search 表函数正确 |

### 4.6 veda-server（HTTP API）

| 测试场景 | 测试名 | 验证点 |
|----------|--------|--------|
| 基础 CRUD | `server_e2e_basic_fs_crud_and_stat` | PUT/GET/DELETE/HEAD/列表 |
| 目录操作 + 复制 + 重命名 + Range | `server_e2e_mkdir_copy_rename_lines_and_range` | mkdir、copy、rename、lines、Range |
| CAS 写入 + Append | `server_e2e_conditional_write_and_append` | If-Match、If-None-Match、append |
| 认证 + 权限 | `server_e2e_auth_and_root_guards` | read-only key、匿名请求、根目录删除保护 |

### 4.7 veda-fuse（内联测试）

| 测试场景 | 文件 | 验证点 |
|----------|------|--------|
| LRU 缓存 | `cache.rs` | put/get/invalidate/eviction |
| 父路径提取 | `sse.rs` | parent_path 函数 |
| SSE 事件解析 | `sse.rs` | JSON 反序列化 |

---

## 5. 常见问题排查

### 5.1 MySQL 连接失败

```
Error: storage error: connection refused
```

**检查：**
1. MySQL 服务是否运行
2. `config/test.toml` 中 database_url 是否正确
3. 用户是否有建表权限

### 5.2 Milvus 搜索不到刚插入的数据

**原因：** Milvus 有索引延迟（通常 <1s）

**解决：** 测试中已有重试逻辑，若仍失败增加等待时间

### 5.3 Embedding API 超时

**检查：**
1. API URL 是否可达
2. API Key 是否有效
3. 模型名称是否正确

### 5.4 测试互相干扰

**解决：** 使用 `--test-threads=1` 串行运行

### 5.5 磁盘空间不足

**检查：** MySQL 数据目录空间

---

## 6. 快速命令参考

```bash
# 快速验证（开发时频繁运行）
cargo test --workspace

# 仅 veda-core（FsService 核心逻辑）
cargo test -p veda-core

# MySQL 集成测试
cargo test -p veda-store -- --ignored

# Server 端到端
cargo test -p veda-server -- --ignored

# 全量集成测试（CI 用）
cargo test --workspace -- --ignored --test-threads=1

# 运行单个测试
cargo test -p veda-core write_and_read
cargo test -p veda-store mysql_migrate_and_dentry_crud -- --ignored

# 显示测试输出
cargo test --workspace -- --nocapture

# 只编译测试（不运行）
cargo test --workspace --no-run
```

---

## 7. CI 配置参考

```yaml
# .github/workflows/test.yml
jobs:
  unit-tests:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - run: cargo test --workspace

  integration-tests:
    runs-on: ubuntu-latest
    services:
      mysql:
        image: mysql:8.0
        env:
          MYSQL_ROOT_PASSWORD: root
          MYSQL_DATABASE: veda
        ports:
          - 3306:3306
      milvus:
        image: milvusdb/milvus:v2.4-latest
        ports:
          - 19530:19530
    steps:
      - uses: actions/checkout@v4
      - run: cp config/test.toml.example config/test.toml
      - run: cargo test --workspace -- --ignored --test-threads=1
```

---

## 8. 附录：测试统计

| Crate | 单元测试 | 集成测试（ignored） | 总计 |
|-------|----------|---------------------|------|
| veda-types | 9 | 0 | 9 |
| veda-core | 47+ | 0 | 47+ |
| veda-store | 0 | 14 | 14 |
| veda-pipeline | 3 | 4 | 7 |
| veda-sql | 45+ | 0 | 45+ |
| veda-server | 0 | 4 | 4 |
| veda-fuse | 7 | 0 | 7 |
| **总计** | **111+** | **22** | **133+** |
