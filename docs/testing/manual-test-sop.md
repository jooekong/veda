# Veda 手动测试 SOP

本文档提供逐条命令的手动测试指南，用于深入理解系统行为。

## 前置准备

### 1. 启动服务器

```bash
# 构建
cargo build -p veda-server -p veda-cli -p veda-fuse

# 启动服务器（新终端）
./target/debug/veda-server config/server.toml
```

### 2. 环境变量（可选）

```bash
# 设置服务器地址（如果不在 localhost:3000）
export VEDA_SERVER="http://localhost:3000"

# 常用 key 存储（测试过程中会获得）
export API_KEY=""
export WORKSPACE_KEY=""
export READ_ONLY_KEY=""
```

### 3. 命令别名

```bash
# CLI
alias veda="./target/debug/veda"

# HTTP 辅助（带认证）
alias hget='f() { curl -s -H "Authorization: Bearer $WORKSPACE_KEY" "$@"; }; f'
alias hput='f() { curl -s -X PUT -H "Authorization: Bearer $WORKSPACE_KEY" "$@"; }; f'
alias hpost='f() { curl -s -X POST -H "Authorization: Bearer $WORKSPACE_KEY" -H "Content-Type: application/json" "$@"; }; f'
alias hdel='f() { curl -s -X DELETE -H "Authorization: Bearer $WORKSPACE_KEY" "$@"; }; f'
```

---

## 一、Account & Workspace 管理

### 1.1 创建账户

```bash
veda account create --name "test-user" --email "test-$(date +%s)@example.com" --password "testpass123"
```

**期望输出：**
```
Account created. API key saved to config.
Account ID: <uuid>
```

**验证：**
```bash
veda config show
```

**后续命令需要 API Key：**
```bash
export API_KEY=$(veda config show | grep "api_key:" | awk '{print $2}')
```

### 1.2 创建 Workspace

```bash
veda workspace create --name "my-workspace"
```

**期望输出：**
```
Workspace created: <workspace-id>
```

记录 Workspace ID：
```bash
export WORKSPACE_ID="<上面输出的 uuid>"
```

### 1.3 创建 Workspace Key（读写权限）

```bash
veda workspace use $WORKSPACE_ID
```

**期望输出：**
```
Workspace <id> selected. Key saved to config.
```

**验证：**
```bash
veda config show
# 此时 workspace_key 应该有值
export WORKSPACE_KEY=$(veda config show | grep "workspace_key:" | awk '{print $2}')
```

### 1.4 创建只读 Workspace Key（需 curl）

CLI 不支持，用 HTTP 直接调用：

```bash
curl -s -X POST "$VEDA_SERVER/v1/workspaces/$WORKSPACE_ID/keys" \
  -H "Authorization: Bearer $API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"permission":"read"}'
```

**期望输出：**
```json
{"success":true,"data":{"key":"wk_xxx","permission":"read"}}
```

**记录只读 Key：**
```bash
export READ_ONLY_KEY="<输出的 key>"
```

### 1.5 列出 Workspaces

```bash
veda workspace list
```

---

## 二、文件 CRUD 操作

### 2.1 写入文件（从本地文件）

```bash
echo "Hello Veda!" > /tmp/test.txt
veda cp /tmp/test.txt /docs/hello.txt
```

**期望输出：**
```
Written: revision 1
```

### 2.2 写入文件（从 stdin）

```bash
echo "Line 1
Line 2
Line 3" | veda cp - /docs/multiline.txt
```

### 2.3 读取文件

```bash
veda cat /docs/hello.txt
```

**期望输出：**
```
Hello Veda!
```

### 2.4 覆盖文件

```bash
echo "Hello Veda v2!" | veda cp - /docs/hello.txt
```

**期望输出：**
```
Written: revision 2
```

> 注意 revision 递增到 2

### 2.5 写入深层路径（自动创建父目录）

```bash
echo "Deep nested content" | veda cp - /a/b/c/d/deep.txt
veda cat /a/b/c/d/deep.txt
```

**验证父目录自动创建：**
```bash
veda ls /a
veda ls /a/b
veda ls /a/b/c
```

### 2.6 删除文件

```bash
veda rm /docs/hello.txt
```

**验证删除后读取失败：**
```bash
veda cat /docs/hello.txt
# 应该报错: read failed: ...
```

### 2.7 读取不存在的文件

```bash
veda cat /nonexistent/path.txt
```

**期望输出：**
```
Error: read failed: ...
```

---

## 三、目录操作

### 3.1 创建目录

```bash
veda mkdir /test-dir
```

### 3.2 创建嵌套目录

```bash
veda mkdir /test-dir/sub/nested/deep
```

> 目录创建是递归的，会自动创建所有父目录

### 3.3 目录幂等性（重复创建不报错）

```bash
veda mkdir /test-dir
# 应该成功，不会报错
```

### 3.4 列出目录内容

```bash
# 先添加一些文件
echo "file1" | veda cp - /test-dir/file1.txt
echo "file2" | veda cp - /test-dir/file2.txt

# 列出目录
veda ls /test-dir
```

**期望输出：**
```
file1.txt
file2.txt
sub/
```

### 3.5 列出根目录

```bash
veda ls /
```

### 3.6 删除目录（递归删除子文件）

```bash
veda rm /test-dir
```

**验证：**
```bash
veda ls /test-dir
# 应报错: ...
```

---

## 四、复制 & 重命名

### 4.1 复制文件（需 curl）

CLI 无此命令，用 HTTP：

```bash
# 准备源文件
echo "source content" | veda cp - /copy-test/src.txt

# 复制
curl -s -X POST "$VEDA_SERVER/v1/fs-copy" \
  -H "Authorization: Bearer $WORKSPACE_KEY" \
  -H "Content-Type: application/json" \
  -d '{"from":"/copy-test/src.txt","to":"/copy-test/dst.txt"}'
```

**验证：**
```bash
veda cat /copy-test/dst.txt
# 应输出: source content
```

### 4.2 重命名文件

```bash
veda mv /copy-test/dst.txt /copy-test/renamed.txt
```

**验证：**
```bash
veda cat /copy-test/renamed.txt
# 应输出: source content

veda cat /copy-test/dst.txt
# 应报错（文件不存在）
```

### 4.3 重命名目录

```bash
veda mv /copy-test /rename-test
veda ls /rename-test
```

---

## 五、Append 操作

### 5.1 Append 到新文件

```bash
veda append /append-test/new.txt "line1"
```

**验证：**
```bash
veda cat /append-test/new.txt
# 输出: line1
```

### 5.2 Append 到已存在文件

```bash
veda append /append-test/new.txt "line2"
veda cat /append-test/new.txt
# 输出: line1line2
```

### 5.3 Append 从 stdin

```bash
echo "line3" | veda append /append-test/new.txt -
veda cat /append-test/new.txt
# 输出: line1line2line3
```

---

## 六、行号读取

### 6.1 准备多行文件

```bash
printf "第一行\n第二行\n第三行\n第四行\n第五行\n" | veda cp - /lines-test/file.txt
```

### 6.2 读取单行

```bash
veda cat /lines-test/file.txt --lines "1:1"
```

**期望输出：**
```
第一行
```

### 6.3 读取多行

```bash
veda cat /lines-test/file.txt --lines "2:4"
```

**期望输出：**
```
第二行
第三行
第四行
```

### 6.4 读取超出 EOF（截断到文件末尾）

```bash
veda cat /lines-test/file.txt --lines "1:100"
```

**期望：** 输出所有 5 行

### 6.5 无效行号范围

```bash
veda cat /lines-test/file.txt --lines "0:5"
# 应报错（行号从 1 开始）

veda cat /lines-test/file.txt --lines "5:3"
# 应报错（start > end）
```

---

## 七、Range 读取（字节范围，需 curl）

### 7.1 准备文件

```bash
echo "0123456789ABCDEF" | veda cp - /range-test/file.bin
```

### 7.2 Range 请求（读取字节 5-10）

```bash
curl -s -H "Authorization: Bearer $WORKSPACE_KEY" \
  -H "Range: bytes=5-10" \
  "$VEDA_SERVER/v1/fs/range-test/file.bin"
```

**期望输出：**
```
56789A
```

### 7.3 开放式 Range（从字节 10 到末尾）

```bash
curl -s -H "Authorization: Bearer $WORKSPACE_KEY" \
  -H "Range: bytes=10-" \
  "$VEDA_SERVER/v1/fs/range-test/file.bin"
```

**期望输出：**
```
ABCDEF
```

### 7.4 无效 Range（超出文件大小）

```bash
curl -s -w "\nHTTP Status: %{http_code}\n" \
  -H "Authorization: Bearer $WORKSPACE_KEY" \
  -H "Range: bytes=100-200" \
  "$VEDA_SERVER/v1/fs/range-test/file.bin"
```

**期望：** HTTP Status: 416 (Range Not Satisfiable)

---

## 八、Stat 操作（元信息查询，需 curl）

### 8.1 文件 Stat

```bash
curl -s -H "Authorization: Bearer $WORKSPACE_KEY" \
  "$VEDA_SERVER/v1/fs/docs/multiline.txt?stat=1" | jq
```

**期望输出：**
```json
{
  "success": true,
  "data": {
    "path": "/docs/multiline.txt",
    "is_dir": false,
    "size_bytes": 20,
    "revision": 1,
    "file_id": "...",
    ...
  }
}
```

### 8.2 目录 Stat

```bash
curl -s -H "Authorization: Bearer $WORKSPACE_KEY" \
  "$VEDA_SERVER/v1/fs/docs?stat=1" | jq
```

**期望：** `is_dir: true`, `file_id: null`

### 8.3 根目录 Stat

```bash
curl -s -H "Authorization: Bearer $WORKSPACE_KEY" \
  "$VEDA_SERVER/v1/fs?stat=1" | jq
```

**期望：** `path: "/"`, `is_dir: true`

### 8.4 Stat 不存在的路径

```bash
curl -s -w "\nHTTP Status: %{http_code}\n" \
  -H "Authorization: Bearer $WORKSPACE_KEY" \
  "$VEDA_SERVER/v1/fs/nonexistent?stat=1"
```

**期望：** HTTP Status: 404

---

## 九、条件写入

### 9.1 If-Match（版本匹配写入）

CLI 的 `write_file` 自动带 `If-None-Match`（内容去重），但 `If-Match`（版本检查）需手动：

```bash
# 写入初始文件
echo "version 1" | veda cp - /cas-test/file.txt

# 获取当前 revision
REVISION=$(curl -s -H "Authorization: Bearer $WORKSPACE_KEY" \
  "$VEDA_SERVER/v1/fs/cas-test/file.txt?stat=1" | jq -r '.data.revision')
echo "Current revision: $REVISION"

# If-Match 成功（版本匹配）
curl -s -w "\nHTTP Status: %{http_code}\n" \
  -H "Authorization: Bearer $WORKSPACE_KEY" \
  -H "If-Match: \"$REVISION\"" \
  -X PUT \
  -d "version 2" \
  "$VEDA_SERVER/v1/fs/cas-test/file.txt"

# 期望: HTTP Status: 200, revision 变为 2
```

### 9.2 If-Match 失败（版本不匹配）

```bash
# 使用旧版本号
curl -s -w "\nHTTP Status: %{http_code}\n" \
  -H "Authorization: Bearer $WORKSPACE_KEY" \
  -H "If-Match: \"1\"" \
  -X PUT \
  -d "should fail" \
  "$VEDA_SERVER/v1/fs/cas-test/file.txt"

# 期望: HTTP Status: 412 (Precondition Failed)
```

### 9.3 If-None-Match（内容去重）

`veda cp` 自动带此 header，写入相同内容时：

```bash
echo "same content" | veda cp - /dedup-test/file.txt
# revision 1

echo "same content" | veda cp - /dedup-test/file.txt
# revision 保持 1，返回 content_unchanged: true
```

**手动验证：**
```bash
SHA256=$(echo -n "same content" | sha256sum | cut -d' ' -f1)

curl -s -H "Authorization: Bearer $WORKSPACE_KEY" \
  -H "If-None-Match: \"$SHA256\"" \
  -X PUT \
  -d "same content" \
  "$VEDA_SERVER/v1/fs/dedup-test/file.txt" | jq

# 期望: {"success":true,"data":{"revision":1,"content_unchanged":true,...}}
```

---

## 十、认证 & 权限

### 10.1 无认证请求

```bash
curl -s -w "\nHTTP Status: %{http_code}\n" "$VEDA_SERVER/v1/fs?list=1"

# 期望: HTTP Status: 401
```

### 10.2 只读 Key 尝试写入

```bash
curl -s -w "\nHTTP Status: %{http_code}\n" \
  -H "Authorization: Bearer $READ_ONLY_KEY" \
  -X PUT \
  -d "test" \
  "$VEDA_SERVER/v1/fs/readonly-test.txt"

# 期望: HTTP Status: 403 (Forbidden)
```

### 10.3 只读 Key 读取

```bash
curl -s -w "\nHTTP Status: %{http_code}\n" \
  -H "Authorization: Bearer $READ_ONLY_KEY" \
  "$VEDA_SERVER/v1/fs/docs/multiline.txt"

# 期望: HTTP Status: 200
```

### 10.4 根目录删除保护

```bash
curl -s -w "\nHTTP Status: %{http_code}\n" \
  -H "Authorization: Bearer $WORKSPACE_KEY" \
  -X DELETE \
  "$VEDA_SERVER/v1/fs"

# 期望: HTTP Status: 400 (Bad Request)
```

---

## 十一、Unicode 路径

### 11.1 中文路径写入

```bash
echo "中文内容测试" | veda cp - "/中文目录/子目录/文件.txt"
veda cat "/中文目录/子目录/文件.txt"

# 列出中文目录
veda ls "/中文目录"
```

### 11.2 中文路径重命名

```bash
veda mv "/中文目录" "/重命名目录"
veda ls "/重命名目录/子目录"
```

### 11.3 Emoji 和特殊字符

```bash
echo "emoji test" | veda cp - "/📁folder/📄file.txt"
veda cat "/📁folder/📄file.txt"
```

---

## 十二、Collection 操作

### 12.1 创建 Collection

```bash
veda collection create "my-collection" \
  --schema '[{"name":"title","field_type":"text"},{"name":"content","field_type":"text"}]' \
  --embed-source "content"
```

### 12.2 列出 Collections

```bash
veda collection list
```

### 12.3 描述 Collection

```bash
veda collection desc "my-collection"
```

### 12.4 插入数据

```bash
veda collection insert "my-collection" '[
  {"title":"First","content":"Hello world"},
  {"title":"Second","content":"Another document"}
]'
```

### 12.5 搜索 Collection

```bash
veda collection search "my-collection" "hello" --limit 5
```

### 12.6 删除 Collection

```bash
veda collection delete "my-collection"
```

---

## 十三、SQL 操作

### 13.1 SELECT 所有文件

```bash
veda sql "SELECT * FROM files LIMIT 10"
```

### 13.2 WHERE 过滤

```bash
veda sql "SELECT path, size_bytes FROM files WHERE path LIKE '/docs/%'"
```

### 13.3 COUNT 聚合

```bash
veda sql "SELECT COUNT(*) as total FROM files"
```

### 13.4 UDF veda_exists

```bash
# 检查文件是否存在
veda sql "SELECT veda_exists('/docs/multiline.txt') as exists"
# 返回 true

veda sql "SELECT veda_exists('/nonexistent.txt') as exists"
# 返回 false
```

### 13.5 UDF veda_read

```bash
veda sql "SELECT veda_read('/docs/multiline.txt') as content"
```

### 13.6 UDF veda_write（写入新文件）

```bash
veda sql "SELECT veda_write('/sql-test/from-sql.txt', 'written from SQL')"
veda cat /sql-test/from-sql.txt
```

### 13.7 UDF veda_append

```bash
veda sql "SELECT veda_append('/sql-test/from-sql.txt', ' appended')"
veda cat /sql-test/from-sql.txt
```

### 13.8 目录操作 UDF

```bash
# mkdir
veda sql "SELECT veda_mkdir('/sql-mkdir-test')"

# remove
veda sql "SELECT veda_remove('/sql-mkdir-test')"
```

---

## 十四、Search 操作

### 14.1 准备可搜索文档

```bash
cat << 'EOF' | veda cp - /search-test/doc.md
# Introduction to Machine Learning

Machine learning is a subset of artificial intelligence.
It enables systems to learn from data without explicit programming.

## Types of Machine Learning

1. Supervised Learning
2. Unsupervised Learning
3. Reinforcement Learning

Deep learning uses neural networks with multiple layers.
EOF

# 等待 worker 处理索引
sleep 5
```

### 14.2 语义搜索

```bash
veda search "neural networks" --mode semantic --limit 5
```

### 14.3 混合搜索

```bash
veda search "machine learning" --mode hybrid --limit 10
```

### 14.4 带过滤的搜索（需 curl）

```bash
curl -s -X POST "$VEDA_SERVER/v1/search" \
  -H "Authorization: Bearer $WORKSPACE_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "query": "learning",
    "mode": "hybrid",
    "limit": 10,
    "where": "path LIKE '/search-test/%'"
  }'
```

---

## 十五、FUSE 挂载

### 15.1 挂载文件系统

```bash
# 创建挂载点
mkdir -p /tmp/veda-fuse

# 挂载（前台运行，便于调试）
VEDA_SERVER="http://localhost:3000" \
VEDA_KEY="$WORKSPACE_KEY" \
./target/debug/veda-fuse mount \
  --foreground \
  --debug \
  /tmp/veda-fuse
```

### 15.2（新终端）FUSE 文件操作

```bash
# 列出文件
ls -la /tmp/veda-fuse/

# 写入文件
echo "FUSE test content" > /tmp/veda-fuse/fuse-test.txt

# 读取文件
cat /tmp/veda-fuse/fuse-test.txt

# 创建目录
mkdir -p /tmp/veda-fuse/fuse-dir/nested

# 列出目录
ls -la /tmp/veda-fuse/fuse-dir/

# 删除文件
rm /tmp/veda-fuse/fuse-test.txt
```

### 15.3 验证同步到服务器

```bash
veda ls /
# 应该能看到 fuse-dir 目录
```

### 15.4 卸载

```bash
# Ctrl+C 停止前台进程，或：
./target/debug/veda-fuse umount /tmp/veda-fuse
```

---

## 十六、内容去重验证

### 16.1 相同内容写入多个文件

```bash
CONTENT="This is identical content for dedup test"

echo "$CONTENT" | veda cp - /dedup-test/file1.txt
echo "$CONTENT" | veda cp - /dedup-test/file2.txt
echo "$CONTENT" | veda cp - /dedup-test/file3.txt
```

### 16.2 验证内容一致

```bash
veda cat /dedup-test/file1.txt
veda cat /dedup-test/file2.txt
veda cat /dedup-test/file3.txt
# 三者内容应完全一致
```

### 16.3 检查 ref_count（底层存储）

```bash
# 查询底层 file 表的 ref_count
veda sql "SELECT file_id, ref_count FROM files WHERE path LIKE '/dedup-test/%'"
# ref_count 应该都是 1

# 但底层 content 表的引用计数应该是 3
# （需要直接查 MySQL）
```

---

## 十七、大文件测试

### 17.1 写入大文件

```bash
# 生成 ~1MB 文件
head -c 1048576 /dev/urandom | base64 > /tmp/large.bin

# 写入
veda cp /tmp/large.bin /large-test/file.bin
```

### 17.2 读取并验证大小

```bash
# 读取
veda cat /large-test/file.bin > /tmp/large-copy.bin

# 比较大小
wc -c /tmp/large.bin /tmp/large-copy.bin
```

---

## 十八、边界情况测试

### 18.1 写入目录路径（应失败）

```bash
veda mkdir /boundary-test

# 尝试写入到已存在的目录路径
echo "test" | veda cp - /boundary-test
# 应报错: already exists
```

### 18.2 重命名到已存在路径（应失败）

```bash
echo "file1" | veda cp - /boundary-test/file1.txt
echo "file2" | veda cp - /boundary-test/file2.txt

veda mv /boundary-test/file1.txt /boundary-test/file2.txt
# 应报错: already exists
```

### 18.3 删除不存在的文件

```bash
veda rm /boundary-test/nonexistent.txt
# 应报错: not found
```

### 18.4 写入空内容

```bash
echo "" | veda cp - /boundary-test/empty.txt
# 应该成功

veda cat /boundary-test/empty.txt
# 输出空行
```

---

## 命令速查表

| 操作 | CLI | HTTP |
|------|-----|------|
| 创建账户 | `veda account create` | `POST /v1/accounts` |
| 创建 Workspace | `veda workspace create` | `POST /v1/workspaces` |
| 创建 Workspace Key | `veda workspace use <id>` | `POST /v1/workspaces/{id}/keys` |
| 写入文件 | `veda cp <src> <dst>` | `PUT /v1/fs/{path}` |
| 读取文件 | `veda cat <path>` | `GET /v1/fs/{path}` |
| 删除文件 | `veda rm <path>` | `DELETE /v1/fs/{path}` |
| 行号读取 | `veda cat <path> --lines 1:10` | `GET /v1/fs/{path}?lines=1:10` |
| 列出目录 | `veda ls <path>` | `GET /v1/fs/{path}?list` |
| 创建目录 | `veda mkdir <path>` | `POST /v1/fs-mkdir` |
| 重命名 | `veda mv <src> <dst>` | `POST /v1/fs-rename` |
| 复制 | ❌ 需 curl | `POST /v1/fs-copy` |
| 追加 | `veda append <path> <content>` | `POST /v1/fs/{path}` |
| Stat | ❌ 需 curl | `GET /v1/fs/{path}?stat=1` |
| Range 读取 | ❌ 需 curl | `GET /v1/fs/{path}` + `Range:` header |
| If-Match | ❌ 需 curl | `PUT /v1/fs/{path}` + `If-Match:` header |
| 搜索 | `veda search <query>` | `POST /v1/search` |
| Collection | `veda collection *` | `POST/GET/DELETE /v1/collections` |
| SQL | `veda sql <query>` | `POST /v1/sql` |
| FUSE 挂载 | `veda-fuse mount` | - |
