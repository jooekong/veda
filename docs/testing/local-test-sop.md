# Veda 本地测试 SOP（全覆盖）

> 一份文档覆盖：环境搭建 → FS（CRUD / 存储 / COW / 配额 / 事件）→ HTTP / SQL / `veda_fs*` → Collection & Embedding → 文件搜索闭环 → FUSE 挂载 → 并发 → 回归测试。

---

## 一、环境

### 0. 前置

- MySQL、Milvus、Embedding API（OpenAI 兼容）可用
- macOS FUSE 测试需 `brew install --cask macfuse`（Linux：`apt install libfuse-dev pkg-config`），否则跳过 §24

### 0.1 编译

```bash
# 主 workspace
cargo build -p veda-server -p veda-cli
alias veda='./target/debug/veda-cli'

# veda-fuse 是独立 crate（在根 Cargo.toml 的 exclude 里），单独构建
cargo build --manifest-path crates/veda-fuse/Cargo.toml
alias vfuse='./crates/veda-fuse/target/debug/veda-fuse'
```

### 1. 启动 Server

```bash
cargo run -p veda-server         # 默认监听 0.0.0.0:3000
```

### 2. 账号 & API Key

```bash
veda config set server_url http://localhost:3000
veda account create --name joe --email joe@test.com --password 123456
# 已有账号用：
# veda account login --email joe@test.com --password 123456
```

### 3. Workspace & Workspace Key

```bash
veda workspace create --name test-ws
veda workspace list
veda workspace use <WORKSPACE_ID>            # 自动创建并保存 read-write key
```

### 4. 只读 Key（给 §11 读-only 测试用）

```bash
WSID=$(awk '/workspace_id/{print $2}' ~/.config/veda/config.toml)
APIK=$(awk '/api_key/{print $2}'      ~/.config/veda/config.toml)

curl -sS -X POST "http://localhost:3000/v1/workspaces/$WSID/keys" \
  -H "Authorization: Bearer $APIK" -H "Content-Type: application/json" \
  -d '{"name":"ro-test","permission":"read"}'
# 用时 veda config set workspace_key <RO_KEY>；测完 veda workspace use $WSID 还原
```

### 配置文件模板 `config/server.toml`

```toml
listen     = "0.0.0.0:3000"
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

### 工作前缀

```bash
export TP=/sop                   # 所有测试路径前缀，结尾清理方便
veda mkdir $TP
```

### 常量参考（`crates/veda-core/src/service/fs.rs`）

| 常量 | 值 | 说明 |
|---|---|---|
| `INLINE_THRESHOLD` | 256 KiB | 超出改走 chunked |
| `CHUNK_SIZE`       | 256 KiB | 尽量在 `\n` 处切块 |
| `MAX_FILE_BYTES`   | 50 MiB  | `write` + `append` 共同配额 |
| segment 上限       | 255 B   | 超长 → `InvalidPath` |
| 禁用字符           | `\0` `:` | 非法 segment |

---

## 二、FS 基础

### 5. 路径规范化

```bash
# 合法：多斜杠、./、NFC
veda sql "SELECT veda_write('$TP/norm//a/./b.txt', 'ok')"
veda sql "SELECT veda_exists('$TP/norm/a/../a/b.txt')"       -- true

# 非法
veda sql "SELECT veda_write('rel.txt', 'x')"                 -- InvalidPath
veda sql "SELECT veda_read('$TP/../../etc/passwd')"          -- InvalidPath
veda sql "SELECT veda_write('$TP/bad:name', 'x')"            -- InvalidPath
# 超长 segment（>255B）
python - <<'PY'
import subprocess, os
subprocess.run(['./target/debug/veda-cli','sql',
    f"SELECT veda_write('{os.environ['TP']}/{'x'*300}', 'x')"])
PY
```

### 6. 基础 CRUD

```bash
# mkdir 幂等
veda mkdir $TP/crud
veda mkdir $TP/crud                                   # 不报错

# 写 / 读 / stat / ls
echo "hello" | veda cp - $TP/crud/a.txt
veda cat $TP/crud/a.txt
veda ls  $TP/crud
curl -sS "http://localhost:3000/v1/fs$TP/crud/a.txt?stat=1" \
  -H "Authorization: Bearer $(awk '/workspace_key/{print $2}' ~/.config/veda/config.toml)"

# stdin 上传
echo "world" | veda cp - $TP/crud/b.txt

# 追加（CLI / stdin / SQL 三种等价路径）
veda append $TP/crud/a.txt " +more"
echo " +stdin" | veda append $TP/crud/a.txt -
veda sql "SELECT veda_append('$TP/crud/a.txt', ' +sql')"
veda cat $TP/crud/a.txt                               -- hello +more +stdin +sql

# 移动 / 删除
veda mv $TP/crud/b.txt $TP/crud/b2.txt
veda rm $TP/crud/a.txt
```

### 7. Revision / Checksum 去重

```bash
echo "same" | veda cp - $TP/rev/x.txt
veda sql "SELECT revision FROM files WHERE path='$TP/rev/x.txt'"     -- 1
echo "same" | veda cp - $TP/rev/x.txt                                # 同内容
veda sql "SELECT revision FROM files WHERE path='$TP/rev/x.txt'"     -- 仍 1
echo "changed" | veda cp - $TP/rev/x.txt
veda sql "SELECT revision FROM files WHERE path='$TP/rev/x.txt'"     -- 2
```

### 8. COW 写隔离

```bash
echo "origin" | veda cp - $TP/cow/src.txt

KEY=$(awk '/workspace_key/{print $2}' ~/.config/veda/config.toml)
curl -sS -X POST http://localhost:3000/v1/fs-copy \
  -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" \
  -d "{\"from\":\"$TP/cow/src.txt\",\"to\":\"$TP/cow/dup.txt\"}"

# 此时 src/dup 共享 file_id
veda sql "SELECT path, file_id FROM files WHERE path LIKE '$TP/cow/%'"

# 覆盖写 dup：fork 出新 file_id，src 内容不变
echo "forked" | veda cp - $TP/cow/dup.txt
veda cat $TP/cow/src.txt                          -- origin

# append 触发 COW 同样生效
curl -sS -X POST http://localhost:3000/v1/fs-copy \
  -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" \
  -d "{\"from\":\"$TP/cow/src.txt\",\"to\":\"$TP/cow/app.txt\"}"
veda append $TP/cow/app.txt " +tail"
veda cat $TP/cow/src.txt                          -- origin
veda cat $TP/cow/app.txt                          -- origin +tail
```

### 9. Ref-count & 级联删除

```bash
echo "shared" | veda cp - $TP/ref/a.txt
for dst in b.txt c.txt; do
  curl -sS -X POST http://localhost:3000/v1/fs-copy \
    -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" \
    -d "{\"from\":\"$TP/ref/a.txt\",\"to\":\"$TP/ref/$dst\"}"
done

veda sql "SELECT file_id, ref_count FROM files WHERE path LIKE '$TP/ref/%'"
veda rm $TP/ref/a.txt
veda rm $TP/ref/b.txt
veda cat $TP/ref/c.txt                            -- 底层仍存在
veda rm $TP/ref/c.txt
veda sql "SELECT COUNT(*) FROM files WHERE path LIKE '$TP/ref/%'"    -- 0
```

### 10. Inline ↔ Chunked

```bash
python -c "open('/tmp/small.txt','w').write('a'*(100*1024))"
python -c "open('/tmp/big.txt','w').write('b'*(300*1024))"

veda cp /tmp/small.txt $TP/store/s.txt
veda cp /tmp/big.txt   $TP/store/b.txt
veda sql "SELECT path, storage_type, size_bytes FROM files WHERE path LIKE '$TP/store/%'"

# 切换：100KB → 300KB inline→chunked；300KB → 100KB chunked→inline
veda cp /tmp/big.txt   $TP/store/s.txt
veda cp /tmp/small.txt $TP/store/b.txt
veda sql "SELECT path, storage_type FROM files WHERE path LIKE '$TP/store/%'"
```

### 11. `cat --lines` 行范围

```bash
printf "one\ntwo\nthree\nfour\nfive\n" | veda cp - $TP/lines/a.txt
veda cat $TP/lines/a.txt --lines 2:4                          -- two/three/four
veda cat $TP/lines/a.txt --lines 5:99                         -- five
veda cat $TP/lines/a.txt --lines 100:200                      -- 空
veda cat $TP/lines/a.txt --lines 0:3                          -- InvalidInput
veda cat $TP/lines/a.txt --lines 5:1                          -- InvalidInput
```

### 12. 错误边界

| 场景 | 命令 | 预期 |
|---|---|---|
| 删根 | `veda rm /` | `cannot delete root` |
| 写到目录路径 | `echo x \| veda cp - $TP/crud` | `is a directory` |
| mkdir 到已存在文件 | `veda mkdir $TP/lines/a.txt` | `exists as a file` |
| rename 到已存在 | `veda mv $TP/lines/a.txt $TP/rev/x.txt` | `AlreadyExists` |
| rename 目录入自身 | `veda mv $TP/crud $TP/crud/sub` | `move a directory into itself` |
| copy 目录 | `fs-copy` src=目录 | `cannot copy a directory` |
| copy src==dst | from==to | `source and destination are the same` |

### 13. 50MB 配额

```bash
python - <<'PY'
from pathlib import Path
Path('/tmp/51mb.txt').write_text('x'*51*1024*1024)
Path('/tmp/49mb.txt').write_text('a'*49*1024*1024)
Path('/tmp/2mb.txt').write_text('b'*2 *1024*1024)
PY

veda cp /tmp/51mb.txt $TP/quota/too-big.txt                    # QuotaExceeded
veda cp /tmp/49mb.txt $TP/quota/base.txt                       # OK
cat /tmp/2mb.txt | veda append $TP/quota/base.txt -            # QuotaExceeded
```

---

## 三、HTTP & SQL

### 14. 原生 HTTP 端点

```bash
KEY=$(awk '/workspace_key/{print $2}' ~/.config/veda/config.toml)
BASE=http://localhost:3000

curl -sS -X PUT    "$BASE/v1/fs$TP/http/a.txt" -H "Authorization: Bearer $KEY" -d 'hi'
curl -sS            "$BASE/v1/fs$TP/http/a.txt"            -H "Authorization: Bearer $KEY"
curl -sS            "$BASE/v1/fs$TP/http/a.txt?stat=1"     -H "Authorization: Bearer $KEY"
curl -sS            "$BASE/v1/fs$TP/http?list=1"           -H "Authorization: Bearer $KEY"
curl -sS            "$BASE/v1/fs$TP/http/a.txt?lines=1:1"  -H "Authorization: Bearer $KEY"
curl -sS -I         "$BASE/v1/fs$TP/http/a.txt"            -H "Authorization: Bearer $KEY"
curl -sS -X POST    "$BASE/v1/fs$TP/http/a.txt" -H "Authorization: Bearer $KEY" -d ' +app'
curl -sS -X DELETE  "$BASE/v1/fs$TP/http/a.txt"            -H "Authorization: Bearer $KEY"
curl -sS -X POST    "$BASE/v1/fs-mkdir"  -H "Authorization: Bearer $KEY" \
  -H "Content-Type: application/json" -d "{\"path\":\"$TP/http/sub\"}"
curl -sS -X POST    "$BASE/v1/fs-rename" -H "Authorization: Bearer $KEY" \
  -H "Content-Type: application/json" -d "{\"from\":\"$TP/http/sub\",\"to\":\"$TP/http/sub2\"}"
```

### 15. `files` 表 SQL

```bash
veda sql "SELECT path, size_bytes, mime_type, revision, storage_type \
          FROM files ORDER BY path LIMIT 20"

veda sql "SELECT path, size_bytes FROM files WHERE path LIKE '$TP/store/%'"

veda sql "SELECT is_dir, COUNT(*) AS cnt, \
          SUM(CASE WHEN size_bytes IS NOT NULL THEN size_bytes ELSE 0 END) AS bytes \
          FROM files GROUP BY is_dir"
```

### 16. FS 标量 UDF

```bash
veda sql "SELECT veda_write ('$TP/udf/a.txt', 'alpha')  AS n"
veda sql "SELECT veda_append('$TP/udf/a.txt', ' beta')  AS n"
veda sql "SELECT veda_read  ('$TP/udf/a.txt')           AS c"     -- alpha beta
veda sql "SELECT veda_exists('$TP/udf/a.txt')           AS b"
veda sql "SELECT veda_size  ('$TP/udf/a.txt')           AS sz"
veda sql "SELECT veda_mtime ('$TP/udf/a.txt')           AS mt"
veda sql "SELECT veda_mkdir ('$TP/udf/sub')             AS ok"
veda sql "SELECT veda_remove('$TP/udf/a.txt')           AS n"

# 组合：对 SELECT 行逐条调用
veda sql "SELECT path, veda_size(path) AS sz \
          FROM files WHERE path LIKE '$TP/store/%'"
```

### 17. `veda_fs()` 表函数

```bash
# 准备数据
veda mkdir $TP/tf; veda mkdir $TP/tf/logs
printf "line1\nline2\nline3\n"                               | veda cp - $TP/tf/notes.txt
printf "name,age\nAlice,30\nBob,25\n"                        | veda cp - $TP/tf/users.csv
printf '{"lvl":"info","msg":"s"}\n{"lvl":"err","msg":"f"}\n' | veda cp - $TP/tf/app.jsonl
printf "a1\na2\n" | veda cp - $TP/tf/logs/a.txt
printf "b1\n"     | veda cp - $TP/tf/logs/b.txt

# 目录（路径 / 结尾）
veda sql "SELECT path, name, type, size_bytes FROM veda_fs('$TP/tf/') ORDER BY path"

# 按扩展名解析
veda sql "SELECT _line_number, line       FROM veda_fs('$TP/tf/notes.txt')"
veda sql "SELECT _line_number, name, age  FROM veda_fs('$TP/tf/users.csv') ORDER BY _line_number"
veda sql "SELECT _line_number, line       FROM veda_fs('$TP/tf/app.jsonl')"

# glob
veda sql "SELECT _path, COUNT(*) n FROM veda_fs('$TP/tf/logs/*.txt') GROUP BY _path"
veda sql "SELECT _path, COUNT(*) n FROM veda_fs('$TP/tf/**/*.txt')   GROUP BY _path"
```

### 18. `veda_fs_events()` 事件流

```bash
veda sql "SELECT veda_write('$TP/ev/a.txt', 'A')"
veda sql "SELECT veda_append('$TP/ev/a.txt', 'A2')"
veda mv  $TP/ev/a.txt $TP/ev/b.txt
veda sql "SELECT veda_remove('$TP/ev/b.txt')"

# 位置参数：(since_id INT, path_prefix STRING, limit INT)
veda sql "SELECT id, event_type, path FROM veda_fs_events() ORDER BY id DESC LIMIT 20"
veda sql "SELECT id, event_type, path FROM veda_fs_events(0, '$TP/ev/', 100)"
veda sql "SELECT id, event_type, path FROM veda_fs_events(1)"

# 错误
veda sql "SELECT * FROM veda_fs_events(0,'$TP/ev/',-1)"       -- limit must be non-negative
veda sql "SELECT * FROM veda_fs_events('oops')"               -- arg 1 (since_id) must be INT
```

预期可见 `create` / `update` / `move` / `delete` 四类事件。

### 19. `veda_storage_stats()`

```bash
veda sql "SELECT total_files, total_directories, total_bytes FROM veda_storage_stats()"
```

### 19.1 只读 Key 权限

```bash
veda config set workspace_key $RO_KEY        # §4 拿到的只读 key

# 读可通过
veda cat $TP/lines/a.txt
veda sql "SELECT veda_read('$TP/lines/a.txt')"

# 写全部拒绝
echo x | veda cp - $TP/ro/denied.txt                 -- permission denied
veda sql "SELECT veda_write('$TP/ro/denied.txt','x')" -- permission denied
veda append $TP/lines/a.txt " x"                     -- permission denied
veda rm $TP/lines/a.txt                              -- permission denied
veda mkdir $TP/ro                                    -- permission denied
veda mv $TP/lines/a.txt $TP/lines/b.txt              -- permission denied

# 还原
veda workspace use $WSID
```

---

## 四、Collection

### 20. 基础管理

```bash
# 新写法：type；旧写法：field_type（同时兼容）
veda collection create docs \
  --schema '[{"name":"title","type":"varchar"},{"name":"content","type":"varchar"},{"name":"category","type":"varchar"}]' \
  --embed-source content

veda collection list
veda collection desc docs

veda collection insert docs '[{"title":"Rust Intro","content":"Rust ownership and borrowing","category":"tech"},{"title":"DB 101","content":"MySQL indexing basics","category":"db"}]'

veda collection search docs "memory management" --limit 5
veda sql "SELECT title, category FROM docs"
veda sql "SELECT category, COUNT(*) FROM docs GROUP BY category"
```

### 21. Embedding 深度

```bash
# kb：语义差异明显的条目
veda collection create kb \
  --schema '[{"name":"title","type":"varchar"},{"name":"content","type":"varchar"},{"name":"domain","type":"varchar"}]' \
  --embed-source content

veda collection insert kb '[
 {"title":"Rust Ownership","content":"Rust uses an ownership system with borrowing and lifetimes to guarantee memory safety without garbage collection.","domain":"programming"},
 {"title":"Photosynthesis","content":"Photosynthesis is the process by which green plants convert sunlight into glucose.","domain":"biology"},
 {"title":"Black Holes","content":"A black hole is a region of spacetime where gravity is so strong that nothing can escape.","domain":"physics"},
 {"title":"SQL Indexing","content":"Database indexes improve the speed of data retrieval. B-tree provides O(log n) lookup.","domain":"database"},
 {"title":"Neural Networks","content":"Neural networks are computing systems inspired by biological neurons, learning via training.","domain":"ml"},
 {"title":"TCP Handshake","content":"TCP uses a three-way handshake SYN/SYN-ACK/ACK to establish connections.","domain":"networking"}]'

veda collection search kb "memory safety and garbage collection"          --limit 3
veda collection search kb "how do plants produce energy from sunlight"    --limit 3
veda collection search kb "speed up database queries"                     --limit 3
veda collection search kb "artificial intelligence training models"       --limit 3
veda collection search kb "data structures used in computer systems"      --limit 6

# embed-source 对比：同一数据、不同 embedding 字段得到不同排序
veda collection create by_title --schema '[{"name":"title","type":"varchar"},{"name":"body","type":"varchar"}]' --embed-source title
veda collection create by_body  --schema '[{"name":"title","type":"varchar"},{"name":"body","type":"varchar"}]' --embed-source body
for C in by_title by_body; do
  veda collection insert $C '[
   {"title":"Quick Sort Algorithm","body":"A restaurant in downtown Shanghai serves excellent xiaolongbao."},
   {"title":"Shanghai Restaurants","body":"Quick sort is a divide-and-conquer sorting algorithm with O(n log n)."}]'
done
veda collection search by_title "sorting algorithm" --limit 2    # top-1: Quick Sort Algorithm
veda collection search by_body  "sorting algorithm" --limit 2    # top-1: Shanghai Restaurants

# 不指定 embed-source：整行 JSON 做 embedding
veda collection create full_embed --schema '[{"name":"key","type":"varchar"},{"name":"value","type":"varchar"}]'
veda collection insert full_embed '[{"key":"color","value":"red"},{"key":"animal","value":"dog"},{"key":"fruit","value":"apple"}]'
veda collection search full_embed "red fruit" --limit 3
```

### 22. 多 Collection + 跨表 JOIN

```bash
veda collection create authors \
  --schema '[{"name":"author","type":"varchar"},{"name":"bio","type":"varchar"},{"name":"country","type":"varchar"}]' \
  --embed-source bio
veda collection create articles \
  --schema '[{"name":"title","type":"varchar"},{"name":"content","type":"varchar"},{"name":"author","type":"varchar"},{"name":"topic","type":"varchar"}]' \
  --embed-source content

veda collection insert authors '[
 {"author":"Alice","bio":"Expert in distributed systems.","country":"US"},
 {"author":"Bob","bio":"Database kernel developer.","country":"CN"},
 {"author":"Carol","bio":"ML researcher focused on NLP.","country":"UK"}]'
veda collection insert articles '[
 {"title":"Raft","content":"Raft is a consensus algorithm.","author":"Alice","topic":"distributed"},
 {"title":"B-Tree","content":"B-Trees are balanced trees for disk I/O.","author":"Bob","topic":"database"},
 {"title":"Attention","content":"Attention focuses on parts of input.","author":"Carol","topic":"ml"},
 {"title":"LSM","content":"LSM trees are write-optimized.","author":"Bob","topic":"database"},
 {"title":"VectorDB","content":"Vector DBs index high-dim embeddings.","author":"Alice","topic":"database"}]'

veda sql "SELECT topic, COUNT(*) cnt FROM articles GROUP BY topic ORDER BY cnt DESC"
veda sql "SELECT a.title, a.author, au.country \
          FROM articles a JOIN authors au ON a.author = au.author"
veda sql "SELECT au.country, COUNT(*) cnt \
          FROM articles a JOIN authors au ON a.author = au.author \
          GROUP BY au.country ORDER BY cnt DESC"
# 三表汇总
veda sql "SELECT 'files' src, COUNT(*) n FROM files \
          UNION ALL SELECT 'articles', COUNT(*) FROM articles \
          UNION ALL SELECT 'authors',  COUNT(*) FROM authors"
```

---

## 五、文件搜索闭环

### 23. Worker ChunkSync + 全局搜索

```bash
echo "Rust is a systems language with memory safety via ownership." \
  | veda cp - $TP/search/rust.md
echo "PostgreSQL is a relational database that supports SQL and JSON." \
  | veda cp - $TP/search/pg.md

sleep 5                          # 等 outbox 消费

veda search "memory safety without GC"           --mode semantic --limit 3
veda search "relational database with JSON"      --mode semantic --limit 3
veda search "PostgreSQL"                         --mode fulltext --limit 3
veda search "ownership"                                           --limit 3   # hybrid
```

预期：每个查询 top-1 命中对应文件。

---

## 六、挂载 & 并发

### 24. FUSE 挂载（需先装 macFUSE/libfuse）

```bash
mkdir -p /tmp/veda-mnt
KEY=$(awk '/workspace_key/{print $2}' ~/.config/veda/config.toml)
vfuse --server http://localhost:3000 --key $KEY --mount /tmp/veda-mnt --foreground &
FUSE_PID=$!
sleep 1

# readdir / getattr / read
ls -la /tmp/veda-mnt$TP
cat    /tmp/veda-mnt$TP/lines/a.txt

# create + write（FUSE create → write → release）
echo "from-fuse" > /tmp/veda-mnt$TP/fuse/hello.txt
veda cat $TP/fuse/hello.txt

# mkdir / rename / truncate / unlink / rmdir
mkdir    /tmp/veda-mnt$TP/fuse/sub
mv       /tmp/veda-mnt$TP/fuse/hello.txt /tmp/veda-mnt$TP/fuse/sub/h2.txt
echo "abcdefghij" > /tmp/veda-mnt$TP/fuse/t.txt
truncate -s 3 /tmp/veda-mnt$TP/fuse/t.txt
veda cat $TP/fuse/t.txt                                   -- abc
rm       /tmp/veda-mnt$TP/fuse/sub/h2.txt
rmdir    /tmp/veda-mnt$TP/fuse/sub

# 卸载
kill $FUSE_PID; wait $FUSE_PID 2>/dev/null
umount /tmp/veda-mnt 2>/dev/null || true
```

### 25. 并发

```bash
# 并发写同一路径
for i in $(seq 1 20); do
  (echo "v$i" | veda cp - $TP/conc/x.txt) &
done; wait
veda sql "SELECT revision FROM files WHERE path='$TP/conc/x.txt'"

# 并发 append
echo "" | veda cp - $TP/conc/log.txt
for i in $(seq 1 20); do
  (veda append $TP/conc/log.txt "line-$i;") &
done; wait
veda cat $TP/conc/log.txt | tr ';' '\n' | grep -c 'line-'      # 应为 20
```

### 26. Outbox / Worker 观察

```bash
veda sql "SELECT event_type, status, COUNT(*) n FROM veda_outbox GROUP BY event_type, status"
# 卡住时：
# UPDATE veda_outbox SET status='completed' WHERE status IN ('pending','failed');
```

---

## 七、回归 & 维护

### 27. 自动化回归

```bash
# Path / FS 核心
cargo test -p veda-core path::tests
cargo test -p veda-core write_file_size_limit
cargo test -p veda-core append_file_cow_isolation
cargo test -p veda-core append_creates_new_file
cargo test -p veda-core append_to_existing_file
cargo test -p veda-core copy_file_shares_content
cargo test -p veda-core delete_cascades_refcount
cargo test -p veda-core rename_into_self

# SQL
cargo test -p veda-sql udf_veda_append
cargo test -p veda-sql udf_veda_read
cargo test -p veda-sql veda_fs_dir_listing
cargo test -p veda-sql veda_fs_read_csv
cargo test -p veda-sql veda_fs_glob
cargo test -p veda-sql veda_fs_events_basic
cargo test -p veda-sql veda_storage_stats_basic
cargo test -p veda-sql read_only_rejects_write_udf

# FUSE（需独立 workspace）
cargo test --manifest-path crates/veda-fuse/Cargo.toml
```

### 28. 清理

```bash
veda rm $TP
for c in docs kb by_title by_body full_embed authors articles; do
  veda collection delete $c
done
veda sql "SELECT COUNT(*) FROM files WHERE path LIKE '$TP/%'"         -- 0
```

### 29. 常见问题

| 问题 | 原因 | 解决 |
|---|---|---|
| `vector dimension mismatch` | Milvus 旧 collection 维度不匹配 | 删除旧 collection 后重启 server |
| `file xxx not found` (Worker) | outbox 残留指向已删文件 | `UPDATE veda_outbox SET status='completed' WHERE status IN ('pending','failed')` |
| `unexpected argument --data` | `data` 是位置参数 | 去掉 `--data`，直接跟 JSON |
| JSON 报 control character | JSON 跨行 | 写在一行内 |
| `permission denied`（写） | 用了只读 key | `veda workspace use $WSID` 还原 |
| `veda_fs_events ... must be INT` | 参数类型错 | 位置参数：`(since_id:INT, path_prefix:STRING, limit:INT)` |
| glob 匹配混合格式 | `*` 同时命中 txt/csv/jsonl | 用更精确 pattern；单次 `veda_fs()` 只查一种格式 |
| veda-fuse 编译错 `package ID ... did not match` | `veda-fuse` 在 workspace exclude | 用 `--manifest-path crates/veda-fuse/Cargo.toml` |
| veda-fuse 编译错 `fuse.pc` 缺失 | 未装 macFUSE/libfuse | `brew install --cask macfuse` 或 `apt install libfuse-dev pkg-config` |

### 30. 源码参考

- HTTP 路由：`crates/veda-server/src/routes/`
- FS 服务：`crates/veda-core/src/service/fs.rs`
- 路径工具：`crates/veda-core/src/path.rs`
- SQL UDF / 表函数：`crates/veda-sql/src/fs_udf.rs`、`fs_table.rs`、`fs_events_table.rs`、`storage_stats_table.rs`
- FUSE：`crates/veda-fuse/src/fs.rs`
- CLI：`crates/veda-cli/src/main.rs`
