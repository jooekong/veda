# Veda 本地测试 SOP

## 前置条件

- MySQL、Milvus、Embedding API 可用
- `config/server.toml` 已配置（见下方模板）

## 0. 编译

```bash
cargo build -p veda-server -p veda-cli
# 后续用 VEDA 代替完整路径
alias veda='./target/debug/veda-cli'
```

## 1. 启动 Server

```bash
cargo run -p veda-server
# 默认监听 0.0.0.0:3000
```

## 2. 配置 CLI

```bash
veda config set server_url http://localhost:3000
veda config show
```

## 3. 创建 Account

```bash
veda account create --name joe --email joe@test.com --password 123456
# API key 自动保存到 ~/.config/veda/config.toml
```

如果 account 已存在：

```bash
veda account login --email joe@test.com --password 123456
```

## 4. 创建 & 选择 Workspace

```bash
veda workspace create --name test-ws
# 输出 workspace ID，复制它

veda workspace list
# 确认 workspace ID

veda workspace use <WORKSPACE_ID>
# workspace key 自动保存
```

## 5. 文件操作

```bash
# 创建目录
veda mkdir /docs

# 上传文件
veda cp ./README.md /docs/readme.md

# 从 stdin 上传
echo "hello world" | veda cp - /docs/hello.txt

# 列目录
veda ls /docs

# 读文件
veda cat /docs/hello.txt

# 移动/重命名
veda mv /docs/hello.txt /docs/greeting.txt

# 删除
veda rm /docs/greeting.txt
```

## 6. 搜索

```bash
# 混合搜索（默认）
veda search "rust ownership"

# 语义搜索
veda search "database indexing" --mode semantic

# 全文搜索
veda search "hello" --mode fulltext --limit 5
```

> 注意：搜索依赖 Worker 完成 ChunkSync，上传文件后等几秒再搜索。

## 7. Collection 操作

```bash
# 创建 collection
veda collection create my_docs \
  --schema '[{"name":"title","field_type":"varchar"},{"name":"content","field_type":"varchar"},{"name":"category","field_type":"varchar"}]' \
  --embed-source content

# 查看 collections
veda collection list

# 插入数据（JSON 必须写在一行）
veda collection insert my_docs '[{"title":"Rust Intro","content":"Rust ownership and borrowing","category":"tech"},{"title":"DB 101","content":"MySQL indexing basics","category":"db"}]'

# 语义搜索 collection
veda collection search my_docs "memory management" --limit 5

# 删除 collection
veda collection delete my_docs
```

## 8. SQL 查询

```bash
# 查文件表
veda sql "SELECT name, size, mime_type FROM files LIMIT 10"

# 查 collection 表（表名 = collection 名）
veda sql "SELECT title, category FROM my_docs LIMIT 20"

# 聚合
veda sql "SELECT category, COUNT(*) AS cnt FROM my_docs GROUP BY category"
```

## 9. 验证配置

```bash
veda config show
# 预期输出：
# server_url: http://localhost:3000
# api_key: <set>
# workspace_id: <uuid>
# workspace_key: <set>
```

## 配置文件模板

`config/server.toml`:

```toml
listen = "0.0.0.0:3000"
jwt_secret = "your-secret-key"

[mysql]
database_url = "mysql://user:pass@host:3306/veda"

[milvus]
url = "http://host:19530"
token = ""
db = "default"

[embedding]
api_url = "https://api.openai.com/v1/embeddings"
api_key = "sk-xxx"
model = "text-embedding-3-small"
dimension = 1024

[worker]
enabled = true
```

## 常见问题


| 问题                            | 原因                        | 解决                                                                               |
| ----------------------------- | ------------------------- | -------------------------------------------------------------------------------- |
| `vector dimension mismatch`   | Milvus 旧 collection 维度不匹配 | 删除旧 collection 后重启 server                                                        |
| `file xxx not found` (Worker) | outbox 中残留指向已删除文件的任务      | `UPDATE veda_outbox SET status='completed' WHERE status IN ('pending','failed')` |
| `unexpected argument --data`  | `data` 是位置参数              | 去掉 `--data`，直接跟 JSON                                                             |
| JSON 解析报 control character    | JSON 字符串跨行了               | 确保 JSON 写在一行内                                                                    |


