# Veda 本地测试 SOP

## 1. 环境准备

### 1.1 依赖服务

| 服务 | 版本 | 用途 | 测试类型 |
|------|------|------|----------|
| MySQL | 8.0+ | 元数据存储 | 集成测试 |
| Milvus | 2.6+ | 向量存储 | 集成测试（可选） |
| Embedding API | OpenAI 兼容 | 文本向量化 | 集成测试（可选） |
| LLM API | OpenAI 兼容 | summary / L0 / L1 生成 | 集成测试（可选，worker/reconciler 用例需要） |

### 1.2 配置文件

```bash
# 复制配置模板
cp config/test.toml.example config/test.toml

# 编辑配置（根据实际环境修改）
vim config/test.toml
```

**config/test.toml 示例（与 `config/test.toml.example` 对齐）：**

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

# LLM provider：单元测试可不配；跑 `cargo test -- --ignored` 中
# 走 LLM 路径的集成测试（worker_atomic_test、reconciler summary 等）必须配。
[llm]
api_url = "https://api.openai.com/v1/chat/completions"
api_key = "sk-xxx"
model = "gpt-4o-mini"
max_summary_tokens = 8192
```

### 1.3 数据库初始化

```bash
# 创建测试数据库
mysql -u root -p -e "CREATE DATABASE IF NOT EXISTS veda CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci;"

# 迁移会在测试时自动执行
```

---

## 2. 测试分类与位置

测试名和数量随重构漂移，本文**不罗列具体测试名**——以各 crate 的 `tests/`
目录为准，想看清单用 `cargo test -p <crate> -- --list`。

约定：集成测试一律标 `#[ignore]`，普通 `cargo test` 只跑无外部依赖的单元测试。

| Crate | 测试位置 | ignored 用例的外部依赖 |
|-------|----------|----------|
| veda-types | `crates/veda-types/tests/` | 无 |
| veda-core | `crates/veda-core/tests/`（FsService / Search，mock store）+ `src/` 内联（path、checksum） | 无 |
| veda-pipeline | `crates/veda-pipeline/tests/` | embedding 用例需 Embedding API |
| veda-sql | `crates/veda-sql/tests/`（mock store） | 无 |
| veda-store | `crates/veda-store/tests/` | `mysql_test` 需 MySQL；`milvus_test` 需 Milvus |
| veda-server | `crates/veda-server/tests/`（server / worker / reconciler / metrics / vectors HTTP 等） | MySQL + Milvus + Embedding，部分需 `[llm]` |
| veda-fuse | `crates/veda-fuse/tests/` + `src/` 内联（cache、sse） | 无（mock HTTP） |

注意：`veda-server` 下的 `remote_e2e_test.rs` 是打**已部署 server** 的黑盒套件
（`VEDA_BASE_URL` 不设则默认打 alpha 部署），见
`docs/testing/e2e-remote-tests.md`。本地集成测试时用 `--test <file>` 指定套件可避开它。

---

## 3. 测试执行命令

### 3.1 快速验证（仅单元测试）

```bash
# 运行所有非 ignored 测试，无需任何外部服务
cargo test --workspace

# 或逐个 crate 运行（更快反馈）
cargo test -p veda-core
cargo test -p veda-sql
```

### 3.2 集成测试（按 crate 指路）

确保对应服务可达且 `config/test.toml` 配置正确：

```bash
# veda-store：MySQL + Milvus
cargo test -p veda-store -- --ignored
cargo test -p veda-store --test mysql_test -- --ignored                      # 仅 MySQL
cargo test -p veda-store --test milvus_test -- --ignored --test-threads=1    # 仅 Milvus（删除可见性有延迟，串行跑）

# veda-pipeline：Embedding API
cargo test -p veda-pipeline -- --ignored

# veda-server：MySQL + Milvus + Embedding（worker/reconciler 用例还需 [llm]）
# in-process build_router 套件，避免连带触发 remote_e2e_test（默认打 alpha 部署）
cargo test -p veda-server --test vectors_http_test -- --ignored
cargo test -p veda-server --test project_data_test -- --ignored
cargo test -p veda-server --test admin_http_test -- --ignored
```

单跑一个测试：

```bash
cargo test -p veda-store mysql_migrate_and_dentry_crud -- --ignored
```

### 3.3 全量集成测试

```bash
# 需 MySQL + Milvus + Embedding API (+ LLM)
cargo test --workspace -- --ignored --test-threads=1
```

---

## 4. 常见问题排查

### 4.1 MySQL 连接失败

```
Error: storage error: connection refused
```

**检查：**
1. MySQL 服务是否运行
2. `config/test.toml` 中 database_url 是否正确
3. 用户是否有建表权限

### 4.2 Milvus 搜索不到刚插入的数据

**原因：** Milvus 有索引延迟（通常 <1s）

**解决：** 测试中已有重试逻辑，若仍失败增加等待时间

### 4.3 Embedding API 超时

**检查：**
1. API URL 是否可达
2. API Key 是否有效
3. 模型名称是否正确

### 4.4 测试互相干扰

**解决：** 使用 `--test-threads=1` 串行运行

### 4.5 磁盘空间不足

**检查：** MySQL 数据目录空间

---

## 5. 快速命令参考

```bash
# 快速验证（开发时频繁运行）
cargo test --workspace

# 仅 veda-core（FsService 核心逻辑）
cargo test -p veda-core

# MySQL 集成测试
cargo test -p veda-store --test mysql_test -- --ignored

# Server 端到端（本地，in-process build_router）
cargo test -p veda-server --test vectors_http_test -- --ignored

# 全量集成测试
cargo test --workspace -- --ignored --test-threads=1

# 列出某 crate 的全部测试
cargo test -p veda-store -- --list

# 显示测试输出
cargo test --workspace -- --nocapture

# 只编译测试（不运行）
cargo test --workspace --no-run
```

---

## 6. CI 说明

仓库 CI（`.github/workflows/` 只有 `release.yml`）**不跑任何测试**。
现行做法：连真实内网 MySQL / Milvus / Embedding，
`cp config/test.toml.example config/test.toml` 后手动 `cargo test -- --ignored`，不起 docker。
