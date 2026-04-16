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

## 10. 复杂测试场景

> 以下场景覆盖多文件 + 多表联合操作、Collection embedding 能力深度验证。
> 请在完成 §3-§4 后按顺序执行。

### 10.1 多目录多文件 + SQL 交叉查询

验证：文件系统深层嵌套 → SQL files 表完整返回元数据。

```bash
# 创建多级目录
veda mkdir /project
veda mkdir /project/src
veda mkdir /project/docs
veda mkdir /project/tests

# 上传不同类型文件
echo "fn main() { println!(\"hello\"); }" | veda cp - /project/src/main.rs
echo "pub fn add(a: i32, b: i32) -> i32 { a + b }" | veda cp - /project/src/lib.rs
echo "# Project README\nThis is a Rust project." | veda cp - /project/docs/readme.md
echo "{ \"name\": \"veda\", \"version\": \"0.1.0\" }" | veda cp - /project/config.json
echo "#[test] fn it_works() { assert_eq!(2+2, 4); }" | veda cp - /project/tests/basic.rs

# 验证目录递归
veda ls /project
veda ls /project/src

# SQL: 查文件元数据是否填充
veda sql "SELECT path, name, size_bytes, mime_type, revision FROM files ORDER BY path"

# SQL: 按目录过滤
veda sql "SELECT path, size_bytes FROM files WHERE path LIKE '/project/src/%'"

# SQL: 统计
veda sql "SELECT is_dir, COUNT(*) AS cnt, SUM(CASE WHEN size_bytes IS NOT NULL THEN size_bytes ELSE 0 END) AS total_bytes FROM files GROUP BY is_dir"
```

预期结果：
- 4 个目录 + 5 个文件 = 9 条 dentry
- 文件行 `size_bytes`、`mime_type` 非 null；目录行为 null
- `/project/src/%` 过滤只返回 `main.rs`, `lib.rs`

### 10.2 多 Collection + SQL 跨表查询

验证：同一 workspace 下多张 Collection 表可以独立查询和 JOIN。

```bash
# 创建两个 collection
veda collection create authors \
  --schema '[{"name":"author","field_type":"varchar"},{"name":"bio","field_type":"varchar"},{"name":"country","field_type":"varchar"}]' \
  --embed-source bio

veda collection create articles \
  --schema '[{"name":"title","field_type":"varchar"},{"name":"content","field_type":"varchar"},{"name":"author","field_type":"varchar"},{"name":"topic","field_type":"varchar"}]' \
  --embed-source content

# Describe 验证 schema
veda collection desc authors
veda collection desc articles

# 插入 authors
veda collection insert authors '[{"author":"Alice","bio":"Expert in distributed systems and consensus algorithms","country":"US"},{"author":"Bob","bio":"Database kernel developer focusing on storage engines","country":"CN"},{"author":"Carol","bio":"Machine learning researcher specializing in NLP and transformers","country":"UK"}]'

# 插入 articles
veda collection insert articles '[{"title":"Raft Consensus","content":"Raft is a consensus algorithm designed for understandability. It decomposes the problem into leader election, log replication, and safety.","author":"Alice","topic":"distributed"},{"title":"B-Tree Internals","content":"B-Trees are balanced tree data structures optimized for disk I/O. They maintain sorted data for efficient range queries.","author":"Bob","topic":"database"},{"title":"Attention Mechanism","content":"The attention mechanism allows models to focus on different parts of the input sequence when producing output.","author":"Carol","topic":"ml"},{"title":"LSM-Tree Storage","content":"Log-structured merge trees are write-optimized data structures widely used in modern databases like RocksDB and LevelDB.","author":"Bob","topic":"database"},{"title":"Vector Databases","content":"Vector databases store and index high-dimensional embeddings for efficient approximate nearest neighbor search.","author":"Alice","topic":"database"}]'

# SQL: 单表查询
veda sql "SELECT title, author, topic FROM articles"
veda sql "SELECT author, country FROM authors"

# SQL: 聚合
veda sql "SELECT topic, COUNT(*) AS cnt FROM articles GROUP BY topic ORDER BY cnt DESC"
veda sql "SELECT author, COUNT(*) AS article_count FROM articles GROUP BY author"

# SQL: WHERE 过滤
veda sql "SELECT title, author FROM articles WHERE topic = 'database'"

# SQL: 跨表 JOIN（authors × articles）
veda sql "SELECT a.title, a.author, au.country FROM articles a JOIN authors au ON a.author = au.author"

# SQL: 跨表 + 聚合
veda sql "SELECT au.country, COUNT(*) AS cnt FROM articles a JOIN authors au ON a.author = au.author GROUP BY au.country ORDER BY cnt DESC"

# SQL: 文件表 + collection 表同时查询
veda sql "SELECT 'files' AS source, COUNT(*) AS cnt FROM files UNION ALL SELECT 'articles', COUNT(*) FROM articles UNION ALL SELECT 'authors', COUNT(*) FROM authors"
```

预期结果：
- `articles` 5 行，`authors` 3 行
- `topic` 聚合：database=3, distributed=1, ml=1
- JOIN 返回 5 行，每篇文章附带作者国家
- UNION ALL 返回 3 行汇总各表行数

### 10.3 Embedding 语义搜索深度测试

验证：embedding 向量正确生成，语义搜索按相关性排序。

```bash
# 创建知识库 collection，embed 字段为 content
veda collection create kb \
  --schema '[{"name":"title","field_type":"varchar"},{"name":"content","field_type":"varchar"},{"name":"domain","field_type":"varchar"}]' \
  --embed-source content

# 插入语义差异明显的条目
veda collection insert kb '[{"title":"Rust Ownership","content":"Rust uses an ownership system with borrowing and lifetimes to guarantee memory safety without garbage collection. Each value has a single owner, and when the owner goes out of scope, the value is dropped.","domain":"programming"},{"title":"Photosynthesis","content":"Photosynthesis is the process by which green plants convert sunlight, water, and carbon dioxide into glucose and oxygen. It occurs in the chloroplasts of plant cells.","domain":"biology"},{"title":"Black Holes","content":"A black hole is a region of spacetime where gravity is so strong that nothing, not even light, can escape from it. They form when massive stars collapse at the end of their life cycle.","domain":"physics"},{"title":"SQL Indexing","content":"Database indexes are data structures that improve the speed of data retrieval operations. B-tree indexes are the most common, providing O(log n) lookup for equality and range queries.","domain":"database"},{"title":"Neural Networks","content":"Neural networks are computing systems inspired by biological neural networks. They consist of layers of interconnected nodes that learn to map inputs to outputs through training on data.","domain":"ml"},{"title":"TCP Handshake","content":"TCP uses a three-way handshake to establish connections: SYN, SYN-ACK, ACK. This ensures both sides are ready to communicate and agree on initial sequence numbers.","domain":"networking"}]'

# 语义搜索：应该按相关性返回
# 查 memory safety → 应优先返回 Rust Ownership
veda collection search kb "memory safety and garbage collection" --limit 3

# 查 plant energy → 应优先返回 Photosynthesis
veda collection search kb "how do plants produce energy from sunlight" --limit 3

# 查 database performance → 应优先返回 SQL Indexing
veda collection search kb "speed up database queries" --limit 3

# 查 deep learning → 应优先返回 Neural Networks
veda collection search kb "artificial intelligence training models" --limit 3

# 跨领域模糊查询：同时涉及多个领域
veda collection search kb "data structures used in computer systems" --limit 6
```

预期结果：
- 每个查询的 top-1 应与意图对应的 domain 匹配
- 跨领域查询返回多个 domain 的结果

### 10.4 Embedding Source 对比测试

验证：不同 `embed-source` 字段影响搜索行为。

```bash
# 创建按 title embedding 的 collection
veda collection create by_title \
  --schema '[{"name":"title","field_type":"varchar"},{"name":"body","field_type":"varchar"}]' \
  --embed-source title

# 创建按 body embedding 的 collection
veda collection create by_body \
  --schema '[{"name":"title","field_type":"varchar"},{"name":"body","field_type":"varchar"}]' \
  --embed-source body

# 相同数据插入两个 collection
veda collection insert by_title '[{"title":"Quick Sort Algorithm","body":"A restaurant in downtown Shanghai serves excellent xiaolongbao dumplings."},{"title":"Shanghai Restaurants","body":"Quick sort is a divide-and-conquer sorting algorithm with average O(n log n) time complexity."}]'

veda collection insert by_body '[{"title":"Quick Sort Algorithm","body":"A restaurant in downtown Shanghai serves excellent xiaolongbao dumplings."},{"title":"Shanghai Restaurants","body":"Quick sort is a divide-and-conquer sorting algorithm with average O(n log n) time complexity."}]'

# 搜索 "sorting algorithm"
# by_title 应优先返回 "Quick Sort Algorithm"（title 匹配）
veda collection search by_title "sorting algorithm" --limit 2

# by_body 应优先返回 "Shanghai Restaurants"（body 内容是排序算法）
veda collection search by_body "sorting algorithm" --limit 2
```

预期结果：
- `by_title` 搜 "sorting algorithm" → top-1 是 "Quick Sort Algorithm"
- `by_body` 搜 "sorting algorithm" → top-1 是 "Shanghai Restaurants"（因为它的 body 才是排序内容）
- 证明 `embed-source` 字段选择直接决定语义搜索方向

### 10.5 无 Embedding Source 的 Collection（全行序列化 Embedding）

验证：不指定 `--embed-source` 时，整行 JSON 做 embedding。

```bash
veda collection create full_embed \
  --schema '[{"name":"key","field_type":"varchar"},{"name":"value","field_type":"varchar"}]'

veda collection insert full_embed '[{"key":"color","value":"red"},{"key":"animal","value":"dog"},{"key":"fruit","value":"apple"}]'

# 搜索应基于整行内容
veda collection search full_embed "red fruit" --limit 3

# SQL 查全量
veda sql "SELECT key, value FROM full_embed"
```

预期结果：
- 搜索 "red fruit" 时 "color:red" 和 "fruit:apple" 排名靠前
- SQL 返回 3 行

### 10.6 文件上传 + 语义搜索端到端

验证：文件上传后 Worker 完成 ChunkSync，全局搜索可以找到文件内容。

```bash
# 上传有意义的长文本
echo "Rust is a systems programming language that runs blazingly fast, prevents segfaults, and guarantees thread safety. It achieves memory safety without garbage collection through its ownership system. The borrow checker ensures references are always valid." | veda cp - /project/docs/rust-intro.md

echo "PostgreSQL is a powerful, open source object-relational database system. It has a strong reputation for reliability, feature robustness, and performance. It supports both SQL and JSON querying." | veda cp - /project/docs/postgres-intro.md

# 等待 Worker 处理
sleep 5

# 语义搜索文件内容
veda search "memory safety without GC" --mode semantic --limit 3
veda search "relational database with JSON support" --mode semantic --limit 3

# 混合搜索
veda search "borrow checker" --limit 3

# 全文搜索
veda search "PostgreSQL" --mode fulltext --limit 3
```

预期结果：
- 语义搜 "memory safety without GC" → 命中 rust-intro.md
- 语义搜 "relational database with JSON support" → 命中 postgres-intro.md
- 全文搜 "PostgreSQL" → 精确命中 postgres-intro.md

### 10.7 清理

```bash
veda collection delete kb
veda collection delete authors
veda collection delete articles
veda collection delete by_title
veda collection delete by_body
veda collection delete full_embed
veda rm /project
```

## 常见问题


| 问题                            | 原因                        | 解决                                                                               |
| ----------------------------- | ------------------------- | -------------------------------------------------------------------------------- |
| `vector dimension mismatch`   | Milvus 旧 collection 维度不匹配 | 删除旧 collection 后重启 server                                                        |
| `file xxx not found` (Worker) | outbox 中残留指向已删除文件的任务      | `UPDATE veda_outbox SET status='completed' WHERE status IN ('pending','failed')` |
| `unexpected argument --data`  | `data` 是位置参数              | 去掉 `--data`，直接跟 JSON                                                             |
| JSON 解析报 control character    | JSON 字符串跨行了               | 确保 JSON 写在一行内                                                                    |


