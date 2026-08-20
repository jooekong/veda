# Veda 手动测试 SOP（fs）

> 本 SOP 2026-06-10 按当前 CLI 重写，合并自旧 `manual-test-sop.md` 与 `local-test-sop.md`，是 fs 手测的唯一入口。
> 逐条命令的手动测试指南，用于深入理解系统行为。db 向量面 / 平台面见 `platform-admin-api-sop.md`。

## 前置准备

### 1. 构建

```bash
cargo build -p veda-server -p veda-cli -p veda-fuse
```

- CLI 二进制名是 **veda**（包名 veda-cli），产物 `target/debug/veda`
- veda-fuse 是普通 workspace member，产物 `target/debug/veda-fuse`
- macOS FUSE 测试需 `brew install --cask macfuse`（Linux：`apt install libfuse-dev pkg-config`），否则跳过 §16

### 2. 启动服务器

```bash
cp config/test.toml.example config/test.toml   # 改成真实内网 MySQL/Milvus/embedding
./target/debug/veda-server config/test.toml    # 默认监听 0.0.0.0:3000；worker 默认开启
```

### 3. 命令别名 & 环境变量

```bash
# CLI
alias veda='./target/debug/veda'

export VEDA_SERVER="http://localhost:3000"

# HTTP 辅助（带认证；key 在 §1.1 拿到后再 export）
alias hget='f() { curl -s -H "Authorization: Bearer $WORKSPACE_KEY" "$@"; }; f'
alias hput='f() { curl -s -X PUT -H "Authorization: Bearer $WORKSPACE_KEY" "$@"; }; f'
alias hpost='f() { curl -s -X POST -H "Authorization: Bearer $WORKSPACE_KEY" -H "Content-Type: application/json" "$@"; }; f'
alias hdel='f() { curl -s -X DELETE -H "Authorization: Bearer $WORKSPACE_KEY" "$@"; }; f'
```

### 4. 关键常量（`crates/veda-core/src/service/fs.rs`、`path.rs`）

| 常量 | 值 | 说明 |
|---|---|---|
| `INLINE_THRESHOLD` | 256 KiB | 超出改走 chunked 存储 |
| `CHUNK_SIZE` | 256 KiB | 尽量在 `\n` 处切块 |
| `MAX_FILE_BYTES` | 50 MiB | write + append 共同配额 |
| segment 上限 | 255 B | 超长 → `InvalidPath` |
| 禁用字符 | `\0` `:` 控制字符 | 非法 segment |

---

## 一、初始化 & Workspace 管理

> 旧命令 `veda account create/login`、`veda workspace create/use` **均已删除**，入口统一为 `veda init` + `veda workspace add/switch/list/rm`。

### 1.1 veda init（一步 onboarding）

```bash
veda init
```

**期望：** 匿名模式下 server 一次往返建好 account + workspace + 两把 key；
`~/.config/veda/config.toml` 写入 `server_url`、`api_key`（vk\_）、`active_workspace = "default"`
和 `[workspaces.default]`（workspace id + wk\_ key）。配置是多 workspace profile 格式，
顶层不再有 `workspace_key`。

五种互斥模式：

| 模式 | 命令 | 用途 |
|---|---|---|
| 匿名 | `veda init` | 零交互，server 直接发账号 |
| 注册 | `veda init --email a@b.c --password xxx` | 新建 email 账号 |
| 登录 | `veda init --login --email a@b.c` | 已有账号接入本机 |
| 升级 | `veda init --upgrade --email a@b.c` | 匿名账号补 email/password |
| 导入 | `veda init --import-key vk_…\|wk_…` | 粘贴他机 key（旧 config 自动备份为 `config.toml.bak.<ts>`） |

**验证：**

```bash
veda status        # 显示 server_url / key 状态 / active workspace + 连通性
```

**提取后续 curl 用的 key**（单 profile 时；多 profile 自行对准 `[workspaces.<alias>]` 块）：

```bash
export API_KEY=$(awk -F'"' '/^api_key = /{print $2}' ~/.config/veda/config.toml)
export WORKSPACE_KEY=$(awk -F'"' '/^key = /{print $2; exit}' ~/.config/veda/config.toml)
export WORKSPACE_ID=$(awk -F'"' '/^id = /{print $2; exit}' ~/.config/veda/config.toml)
```

### 1.2 Workspace profile 管理（本地 alias 模型）

```bash
veda workspace add scratch                      # 服务端新建 workspace + 本地 alias，自动 mint wk_
veda workspace add shared --workspace-id $WORKSPACE_ID   # 给已有 workspace 再配一把 key
veda workspace list                             # 列【本地 profile】，active 标 ★
veda workspace switch scratch                   # 切 active profile
veda workspace switch default
veda workspace rm scratch                       # 只删本地 alias，不 revoke 服务端 wk_
```

- `veda ws` 是 `veda workspace` 的别名
- 任意数据命令可用全局 `--workspace <alias>` 临时切 profile（不改配置）
- `veda config set` 只接受 `server_url` / `api_key` / `active_workspace` 三个键，
  workspace 条目一律走 `veda workspace add`

### 1.3 创建只读 Workspace Key（需 curl）

CLI 不直接支持，用控制面 HTTP（Bearer 是 vk\_）：

```bash
curl -s -X POST "$VEDA_SERVER/v1/workspaces/$WORKSPACE_ID/keys" \
  -H "Authorization: Bearer $API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"name":"ro-test","permission":"read"}'
```

**期望输出：** `{"success":true,"data":{"key":"wk_xxx","permission":"read",...}}`

```bash
export READ_ONLY_KEY="<输出的 key>"
```

> 只读 key 的读/写测试全部走 curl（§10）——config 里塞不进第二把 wk\_ 的旧做法已失效。

---

## 二、文件 CRUD 操作

> `veda rm` 在 TTY 上会要求 `[y/N]` 确认；stdin 非 TTY（脚本/管道）时只在 stderr 公告不阻塞。

### 2.1 写入文件（从本地文件）

```bash
echo "Hello Veda!" > /tmp/test.txt
veda cp /tmp/test.txt /docs/hello.txt
```

**期望输出：** `Written: revision 1`

### 2.2 写入文件（从 stdin）

```bash
echo "Line 1
Line 2
Line 3" | veda cp - /docs/multiline.txt
```

### 2.3 读取文件

```bash
veda cat /docs/hello.txt
# Hello Veda!
```

### 2.4 覆盖文件

```bash
echo "Hello Veda v2!" | veda cp - /docs/hello.txt
# Written: revision 2   ← revision 递增
```

### 2.5 写入深层路径（自动创建父目录）

```bash
echo "Deep nested content" | veda cp - /a/b/c/d/deep.txt
veda cat /a/b/c/d/deep.txt
veda ls /a/b/c
```

### 2.6 删除文件

```bash
veda rm /docs/hello.txt
veda cat /docs/hello.txt     # 应报错: read failed / not found
```

### 2.7 读取不存在的文件

```bash
veda cat /nonexistent/path.txt    # 应报错
```

---

## 三、目录操作

```bash
veda mkdir /test-dir                       # 创建
veda mkdir /test-dir/sub/nested/deep       # 递归创建
veda mkdir /test-dir                       # 幂等，不报错

echo "file1" | veda cp - /test-dir/file1.txt
echo "file2" | veda cp - /test-dir/file2.txt
veda ls /test-dir                          # file1.txt file2.txt sub/
veda ls /                                  # 根目录

veda rm /test-dir                          # 递归删除（含子文件）
veda ls /test-dir                          # 应报错
```

---

## 四、复制 & 重命名

### 4.1 复制文件（需 curl，CLI 无此命令）

```bash
echo "source content" | veda cp - /copy-test/src.txt

curl -s -X POST "$VEDA_SERVER/v1/fs-copy" \
  -H "Authorization: Bearer $WORKSPACE_KEY" \
  -H "Content-Type: application/json" \
  -d '{"from":"/copy-test/src.txt","to":"/copy-test/dst.txt"}'

veda cat /copy-test/dst.txt    # source content
```

### 4.2 重命名文件

```bash
veda mv /copy-test/dst.txt /copy-test/renamed.txt
veda cat /copy-test/renamed.txt   # source content
veda cat /copy-test/dst.txt       # 应报错（已不存在）
```

### 4.3 重命名目录

```bash
veda mv /copy-test /rename-test
veda ls /rename-test
```

---

## 五、Append 操作

```bash
veda append /append-test/new.txt "line1"      # append 不存在的路径 → 创建
veda cat /append-test/new.txt                 # line1

veda append /append-test/new.txt "line2"
veda cat /append-test/new.txt                 # line1line2

echo "line3" | veda append /append-test/new.txt -   # stdin
veda cat /append-test/new.txt                 # line1line2line3
```

---

## 六、行号读取（`--range` / `--head` / `--tail`）

> 旧 `--lines` 已删。三个 flag 互斥；HTTP 侧仍是 `?lines=1:10`。

### 6.1 准备多行文件

```bash
printf "第一行\n第二行\n第三行\n第四行\n第五行\n" | veda cp - /lines-test/file.txt
```

### 6.2 范围读取

```bash
veda cat /lines-test/file.txt --range 1:1     # 第一行
veda cat /lines-test/file.txt --range 2:4     # 第二行/第三行/第四行
veda cat /lines-test/file.txt --range 1:100   # 截断到 EOF，输出全部 5 行
veda cat /lines-test/file.txt --range 3:      # 开区间：第 3 行到 EOF
```

### 6.3 head / tail

```bash
veda cat /lines-test/file.txt --head 2        # 前 2 行（等价 --range 1:2）
veda cat /lines-test/file.txt --tail 2        # 后 2 行（拉全文后本地切片）
```

### 6.4 无效行号范围

```bash
veda cat /lines-test/file.txt --range 0:5     # 应报错（行号从 1 开始）
veda cat /lines-test/file.txt --range 5:3     # 应报错（start > end）
```

---

## 七、Range 读取（字节范围，需 curl）

```bash
echo "0123456789ABCDEF" | veda cp - /range-test/file.bin

# 读取字节 5-10
curl -s -H "Authorization: Bearer $WORKSPACE_KEY" \
  -H "Range: bytes=5-10" \
  "$VEDA_SERVER/v1/fs/range-test/file.bin"
# 56789A

# 开放式 Range（字节 10 到末尾）
curl -s -H "Authorization: Bearer $WORKSPACE_KEY" \
  -H "Range: bytes=10-" \
  "$VEDA_SERVER/v1/fs/range-test/file.bin"
# ABCDEF

# 超出文件大小
curl -s -w "\nHTTP Status: %{http_code}\n" \
  -H "Authorization: Bearer $WORKSPACE_KEY" \
  -H "Range: bytes=100-200" \
  "$VEDA_SERVER/v1/fs/range-test/file.bin"
# HTTP Status: 416 (Range Not Satisfiable)
```

---

## 八、Stat 操作（元信息查询，需 curl）

```bash
# 文件
curl -s -H "Authorization: Bearer $WORKSPACE_KEY" \
  "$VEDA_SERVER/v1/fs/docs/multiline.txt?stat=1" | jq
# data: path / is_dir=false / size_bytes / revision / file_id ...

# 目录
curl -s -H "Authorization: Bearer $WORKSPACE_KEY" \
  "$VEDA_SERVER/v1/fs/docs?stat=1" | jq
# is_dir=true, file_id=null

# 根目录
curl -s -H "Authorization: Bearer $WORKSPACE_KEY" \
  "$VEDA_SERVER/v1/fs?stat=1" | jq
# path="/", is_dir=true

# 不存在的路径
curl -s -w "\nHTTP Status: %{http_code}\n" \
  -H "Authorization: Bearer $WORKSPACE_KEY" \
  "$VEDA_SERVER/v1/fs/nonexistent?stat=1"
# HTTP Status: 404
```

---

## 九、条件写入

### 9.1 If-Match（版本匹配写入）

```bash
echo "version 1" | veda cp - /cas-test/file.txt

REVISION=$(curl -s -H "Authorization: Bearer $WORKSPACE_KEY" \
  "$VEDA_SERVER/v1/fs/cas-test/file.txt?stat=1" | jq -r '.data.revision')

# 版本匹配 → 200，revision +1
curl -s -w "\nHTTP Status: %{http_code}\n" \
  -H "Authorization: Bearer $WORKSPACE_KEY" \
  -H "If-Match: \"$REVISION\"" \
  -X PUT -d "version 2" \
  "$VEDA_SERVER/v1/fs/cas-test/file.txt"

# 旧版本号 → 412 Precondition Failed
curl -s -w "\nHTTP Status: %{http_code}\n" \
  -H "Authorization: Bearer $WORKSPACE_KEY" \
  -H "If-Match: \"1\"" \
  -X PUT -d "should fail" \
  "$VEDA_SERVER/v1/fs/cas-test/file.txt"
```

### 9.2 If-None-Match（内容去重）

`veda cp` 自动带此 header，写入相同内容时 revision 不变并返回 `content_unchanged: true`：

```bash
echo "same content" | veda cp - /dedup-test/file.txt    # revision 1
echo "same content" | veda cp - /dedup-test/file.txt    # 仍 revision 1

# 手动验证
SHA256=$(echo -n "same content" | sha256sum | cut -d' ' -f1)
curl -s -H "Authorization: Bearer $WORKSPACE_KEY" \
  -H "If-None-Match: \"$SHA256\"" \
  -X PUT -d "same content" \
  "$VEDA_SERVER/v1/fs/dedup-test/file.txt" | jq
# {"success":true,"data":{"revision":1,"content_unchanged":true,...}}
```

---

## 十、认证 & 权限

```bash
# 无认证 → 401
curl -s -w "\nHTTP Status: %{http_code}\n" "$VEDA_SERVER/v1/fs?list=1"

# 只读 key 写入 → 403
curl -s -w "\nHTTP Status: %{http_code}\n" \
  -H "Authorization: Bearer $READ_ONLY_KEY" \
  -X PUT -d "test" "$VEDA_SERVER/v1/fs/readonly-test.txt"

# 只读 key 读取 → 200
curl -s -w "\nHTTP Status: %{http_code}\n" \
  -H "Authorization: Bearer $READ_ONLY_KEY" \
  "$VEDA_SERVER/v1/fs/docs/multiline.txt"

# 根目录删除保护 → 400
curl -s -w "\nHTTP Status: %{http_code}\n" \
  -H "Authorization: Bearer $WORKSPACE_KEY" \
  -X DELETE "$VEDA_SERVER/v1/fs"

# 只读 key 走 SQL 写 UDF → permission denied
curl -s -X POST "$VEDA_SERVER/v1/sql" \
  -H "Authorization: Bearer $READ_ONLY_KEY" \
  -H "Content-Type: application/json" \
  -d '{"sql":"SELECT veda_write('/ro/denied.txt', 'x')"}'
```

---

## 十一、Unicode 路径

```bash
echo "中文内容测试" | veda cp - "/中文目录/子目录/文件.txt"
veda cat "/中文目录/子目录/文件.txt"
veda ls "/中文目录"

veda mv "/中文目录" "/重命名目录"
veda ls "/重命名目录/子目录"

echo "emoji test" | veda cp - "/📁folder/📄file.txt"
veda cat "/📁folder/📄file.txt"
```

---

## 十二、FS 语义边界

### 12.1 路径规范化

```bash
# 合法：多斜杠、./、..（解析后仍在 workspace 内）
veda sql "SELECT veda_write('/edge/norm//a/./b.txt', 'ok')"
veda sql "SELECT veda_exists('/edge/norm/a/../a/b.txt')"      -- true

# 非法
veda sql "SELECT veda_write('rel.txt', 'x')"                  -- InvalidPath（相对路径）
veda sql "SELECT veda_read('/edge/../../etc/passwd')"         -- InvalidPath（越界）
veda sql "SELECT veda_write('/edge/bad:name', 'x')"           -- InvalidPath（: 禁用）
```

### 12.2 Revision / Checksum 去重（SQL 观察）

```bash
echo "same" | veda cp - /edge/rev/x.txt
veda sql "SELECT revision FROM files WHERE path='/edge/rev/x.txt'"    -- 1
echo "same" | veda cp - /edge/rev/x.txt
veda sql "SELECT revision FROM files WHERE path='/edge/rev/x.txt'"    -- 仍 1（去重）
echo "changed" | veda cp - /edge/rev/x.txt
veda sql "SELECT revision FROM files WHERE path='/edge/rev/x.txt'"    -- 2
```

### 12.3 COW 写隔离

```bash
echo "origin" | veda cp - /edge/cow/src.txt
hpost "$VEDA_SERVER/v1/fs-copy" -d '{"from":"/edge/cow/src.txt","to":"/edge/cow/dup.txt"}'

# 此时 src/dup 共享 file_id
veda sql "SELECT path, file_id FROM files WHERE path LIKE '/edge/cow/%'"

# 覆盖写 dup：fork 出新 file_id，src 内容不变
echo "forked" | veda cp - /edge/cow/dup.txt
veda cat /edge/cow/src.txt                        -- origin

# append 同样触发 COW
hpost "$VEDA_SERVER/v1/fs-copy" -d '{"from":"/edge/cow/src.txt","to":"/edge/cow/app.txt"}'
veda append /edge/cow/app.txt " +tail"
veda cat /edge/cow/src.txt                        -- origin
veda cat /edge/cow/app.txt                        -- origin +tail
```

### 12.4 Ref-count & 级联删除

```bash
echo "shared" | veda cp - /edge/ref/a.txt
for dst in b.txt c.txt; do
  hpost "$VEDA_SERVER/v1/fs-copy" -d "{\"from\":\"/edge/ref/a.txt\",\"to\":\"/edge/ref/$dst\"}"
done

veda sql "SELECT file_id, ref_count FROM files WHERE path LIKE '/edge/ref/%'"
veda rm /edge/ref/a.txt
veda rm /edge/ref/b.txt
veda cat /edge/ref/c.txt                          -- 底层内容仍在
veda rm /edge/ref/c.txt
veda sql "SELECT COUNT(*) FROM files WHERE path LIKE '/edge/ref/%'"   -- 0
```

### 12.5 Inline ↔ Chunked 切换

```bash
python3 -c "open('/tmp/small.txt','w').write('a'*(100*1024))"
python3 -c "open('/tmp/big.txt','w').write('b'*(300*1024))"

veda cp /tmp/small.txt /edge/store/s.txt
veda cp /tmp/big.txt   /edge/store/b.txt
veda sql "SELECT path, storage_type, size_bytes FROM files WHERE path LIKE '/edge/store/%'"
# s.txt=inline (100KB < 256KB)，b.txt=chunked

# 互换：inline→chunked、chunked→inline
veda cp /tmp/big.txt   /edge/store/s.txt
veda cp /tmp/small.txt /edge/store/b.txt
veda sql "SELECT path, storage_type FROM files WHERE path LIKE '/edge/store/%'"
```

### 12.6 错误边界

| 场景 | 命令 | 预期 |
|---|---|---|
| 删根 | `veda rm /` | `cannot delete root` |
| 写到目录路径 | `echo x \| veda cp - /edge/store` | already exists / is a directory |
| mkdir 到已存在文件 | `veda mkdir /lines-test/file.txt` | exists as a file |
| rename 到已存在 | `veda mv /a /lines-test/file.txt` | `AlreadyExists` |
| rename 目录入自身 | `veda mv /edge /edge/sub` | move a directory into itself |
| copy 目录 | `fs-copy` src=目录 | cannot copy a directory |
| copy src==dst | from==to | source and destination are the same |
| 删除不存在 | `veda rm /nope.txt` | not found |
| 空内容写入 | `echo "" \| veda cp - /edge/empty.txt` | 成功 |
| 超长 segment（>255B） | 路径单段 300 字符 | `InvalidPath` |

### 12.7 50MB 配额

```bash
python3 - <<'PY'
from pathlib import Path
Path('/tmp/51mb.txt').write_text('x'*51*1024*1024)
Path('/tmp/49mb.txt').write_text('a'*49*1024*1024)
Path('/tmp/2mb.txt').write_text('b'*2*1024*1024)
PY

veda cp /tmp/51mb.txt /edge/quota/too-big.txt            # QuotaExceeded
veda cp /tmp/49mb.txt /edge/quota/base.txt               # OK
cat /tmp/2mb.txt | veda append /edge/quota/base.txt -    # QuotaExceeded（write+append 共享配额）
```

---

## 十三、Collection 操作

> 字段类型键：新写法 `type`，旧写法 `field_type` 同时兼容（serde alias）。
> 类型值 `int/int64/float/double/bool` 之外的字符串（如 `varchar`）都落到 Milvus VarChar。

```bash
# 创建（--embed-source 指定 embedding 字段；不指定则整行 JSON 做 embedding）
veda collection create docs \
  --schema '[{"name":"title","type":"varchar"},{"name":"content","type":"varchar"},{"name":"category","type":"varchar"}]' \
  --embed-source content

veda collection list
veda collection desc docs

# 插入（data 是位置参数，不要写 --data）
veda collection insert docs '[{"title":"Rust Intro","content":"Rust ownership and borrowing","category":"tech"},{"title":"DB 101","content":"MySQL indexing basics","category":"db"}]'

# 语义搜索
veda collection search docs "memory management" --limit 5

# SQL 直查 collection 表
veda sql "SELECT title, category FROM docs"
veda sql "SELECT category, COUNT(*) FROM docs GROUP BY category"

# 删除
veda collection delete docs
```

---

## 十四、SQL 操作

### 14.1 files 表

```bash
veda sql "SELECT * FROM files LIMIT 10"
veda sql "SELECT path, size_bytes, mime_type, revision, storage_type FROM files WHERE path LIKE '/docs/%'"
veda sql "SELECT COUNT(*) as total FROM files"
veda sql "SELECT is_dir, COUNT(*) AS cnt FROM files GROUP BY is_dir"
```

### 14.2 FS 标量 UDF

```bash
veda sql "SELECT veda_write ('/sql-test/a.txt', 'alpha')  AS n"
veda sql "SELECT veda_append('/sql-test/a.txt', ' beta')  AS n"
veda sql "SELECT veda_read  ('/sql-test/a.txt')           AS c"    -- alpha beta
veda sql "SELECT veda_exists('/sql-test/a.txt')           AS b"    -- true
veda sql "SELECT veda_size  ('/sql-test/a.txt')           AS sz"
veda sql "SELECT veda_mtime ('/sql-test/a.txt')           AS mt"
veda sql "SELECT veda_mkdir ('/sql-test/sub')             AS ok"
veda sql "SELECT veda_remove('/sql-test/a.txt')           AS n"

# 组合：对 SELECT 行逐条调用（列作为 UDF 参数）
veda sql "SELECT path, veda_size(path) AS sz FROM files WHERE path LIKE '/edge/store/%'"
```

### 14.3 `veda_fs()` 表函数

```bash
# 准备数据
veda mkdir /tf; veda mkdir /tf/logs
printf "line1\nline2\nline3\n"                               | veda cp - /tf/notes.txt
printf "name,age\nAlice,30\nBob,25\n"                        | veda cp - /tf/users.csv
printf '{"lvl":"info","msg":"s"}\n{"lvl":"err","msg":"f"}\n' | veda cp - /tf/app.jsonl
printf "a1\na2\n" | veda cp - /tf/logs/a.txt
printf "b1\n"     | veda cp - /tf/logs/b.txt

# 目录列表（路径 / 结尾）
veda sql "SELECT path, name, type, size_bytes FROM veda_fs('/tf/') ORDER BY path"

# 按扩展名解析
veda sql "SELECT _line_number, line       FROM veda_fs('/tf/notes.txt')"
veda sql "SELECT _line_number, name, age  FROM veda_fs('/tf/users.csv') ORDER BY _line_number"
veda sql "SELECT _line_number, line       FROM veda_fs('/tf/app.jsonl')"

# glob（单次 veda_fs() 只查一种格式，pattern 别混格式）
veda sql "SELECT _path, COUNT(*) n FROM veda_fs('/tf/logs/*.txt') GROUP BY _path"
veda sql "SELECT _path, COUNT(*) n FROM veda_fs('/tf/**/*.txt')   GROUP BY _path"
```

### 14.4 `veda_fs_events()` 事件流

```bash
veda sql "SELECT veda_write('/ev/a.txt', 'A')"
veda mv /ev/a.txt /ev/b.txt
veda sql "SELECT veda_remove('/ev/b.txt')"

# 位置参数：(since_id INT, path_prefix STRING, limit INT)
veda sql "SELECT id, event_type, path FROM veda_fs_events() ORDER BY id DESC LIMIT 20"
veda sql "SELECT id, event_type, path FROM veda_fs_events(0, '/ev/', 100)"

# 错误用例
veda sql "SELECT * FROM veda_fs_events(0,'/ev/',-1)"      -- limit must be non-negative
veda sql "SELECT * FROM veda_fs_events('oops')"           -- arg 1 (since_id) must be INT
```

预期可见 `create` / `update` / `move` / `delete` 四类事件。

### 14.5 `veda_storage_stats()`

```bash
veda sql "SELECT total_files, total_directories, total_bytes FROM veda_storage_stats()"
```

---

## 十五、Search 操作

### 15.1 准备可搜索文档

```bash
cat << 'EOF' | veda cp - /search-test/doc.md
# Introduction to Machine Learning

Machine learning is a subset of artificial intelligence.
Deep learning uses neural networks with multiple layers.
EOF

sleep 5    # 等 worker 消费 outbox 建索引
```

### 15.2 三种模式

```bash
veda search "neural networks" --mode semantic --limit 5
veda search "machine learning" --mode fulltext --limit 5
veda search "machine learning" --limit 10               # 默认 hybrid
```

### 15.3 路径过滤

```bash
# CLI：--path 限定子树
veda search "learning" --path /search-test

# HTTP：body 字段是 path_prefix（没有 where 字段）
curl -s -X POST "$VEDA_SERVER/v1/search" \
  -H "Authorization: Bearer $WORKSPACE_KEY" \
  -H "Content-Type: application/json" \
  -d '{"query":"learning","mode":"hybrid","limit":10,"path_prefix":"/search-test"}'
```

---

## 十六、FUSE 挂载

### 16.1 挂载

```bash
mkdir -p /tmp/veda-fuse

# 子命令 + 位置参数（没有 --mount flag）
./target/debug/veda-fuse mount \
  --server http://localhost:3000 \
  --key $WORKSPACE_KEY \
  /tmp/veda-fuse \
  --foreground
```

- 0.1.12 起 `--server` / `--key` 可省略：依次回退 `$VEDA_SERVER` / `$VEDA_KEY`，再回退
  `~/.config/veda/config.toml`（active workspace 的 key）
- `--workspace <alias>` 选非 active 的 profile（显式给了 `--key` 时无效）
- 调试加 `--debug`；写模式 `--write-mode sync|writeback`

### 16.2（新终端）文件操作

```bash
ls -la /tmp/veda-fuse/
echo "FUSE test content" > /tmp/veda-fuse/fuse-test.txt
cat /tmp/veda-fuse/fuse-test.txt
mkdir -p /tmp/veda-fuse/fuse-dir/nested
mv /tmp/veda-fuse/fuse-test.txt /tmp/veda-fuse/fuse-dir/renamed.txt
rm /tmp/veda-fuse/fuse-dir/renamed.txt
```

### 16.3 验证同步到服务器

```bash
veda ls /          # 应能看到 fuse-dir
```

### 16.4 卸载

```bash
# Ctrl+C 停止前台进程，或：
./target/debug/veda-fuse umount /tmp/veda-fuse
```

---

## 十七、内容去重验证

```bash
CONTENT="This is identical content for dedup test"
echo "$CONTENT" | veda cp - /dedup-test/file1.txt
echo "$CONTENT" | veda cp - /dedup-test/file2.txt
echo "$CONTENT" | veda cp - /dedup-test/file3.txt

# 三者内容一致；files 表各自 ref_count=1，
# 底层 content 行被三个 file 共享（需直连 MySQL 看 veda_file_contents）
veda sql "SELECT file_id, ref_count FROM files WHERE path LIKE '/dedup-test/%'"
```

---

## 十八、大文件测试

```bash
head -c 1048576 /dev/urandom | base64 > /tmp/large.bin
veda cp /tmp/large.bin /large-test/file.bin
veda cat /large-test/file.bin > /tmp/large-copy.bin
wc -c /tmp/large.bin /tmp/large-copy.bin     # 大小一致
```

---

## 十九、Outbox / Worker 观察（直连 MySQL）

> veda-sql 引擎里**没有** `veda_outbox` 表（只有 `files`、collection 表和
> `veda_fs` / `veda_fs_events` / `veda_storage_stats` / `search` 表函数）。
> 查 outbox 必须直连 MySQL（连接串见 `config/test.toml` 的 `[mysql].database_url`）：

```bash
mysql -h <host> -u <user> -p<password> veda \
  -e "SELECT event_type, status, COUNT(*) n FROM veda_outbox GROUP BY event_type, status;"

# worker 卡住时（确认无在跑任务再操作）：
# UPDATE veda_outbox SET status='completed' WHERE status IN ('pending','failed');
```

---

## 二十、回归测试（指路式）

测试名随重构变化，跑整文件而非记名字；个别示例名以 grep 现状为准：

```bash
# FS 核心（COW / append / lines / refcount 边界都在这个文件）
cargo test -p veda-core --test fs_service_test
# 示例：copy_file_cow / copy_overwrite_decrements_old_ref_count / delete_dir_cleans_up_child_files

# SQL UDF / 表函数
cargo test -p veda-sql
# 示例：udf_veda_append / veda_fs_dir_listing / veda_fs_read_csv / veda_fs_glob /
#       veda_fs_events_basic / veda_storage_stats_basic / read_only_rejects_write_udf

# FUSE（workspace member，正常 -p 即可）
cargo test -p veda-fuse
```

集成测试全集与环境配置见 `docs/testing/test-sop.md`。

---

## 二十一、记忆浏览页（web console，M4a）

前置：浏览器打开 console，workspace 列表行点「记忆」进 `#/console/memory/{ws}`，
输入该 workspace 的 `wk_`（与「文件」页共用一份，存当前标签页）。

1. **团队页签**：默认打开即列出团队记忆（`updated_at` 倒序）；「+ 添一条」
   写入后立刻出现在列表；再存一条语义相近的 → 表单下方出现「已有相似记忆」
   提示（近邻引导）；一字不差重存 → 提示「相同内容已存在」。
2. **主题目录**：左侧按主题计数；点主题过滤右侧列表；无 topic 的行在「未分类」。
3. **行内改/删**：点「改」改正文/主题/到期日 → 署名（updated_by）变为当前身份，
   带到期日的行显示「到期」角标（清空到期日不支持，删了重存——M1 拍板）；
   点「删」确认后行消失，且 `veda ask` / bot 立即检索不到（硬删）。
4. **身份栏**：填 `wecom:<自己的企微id>` 点确定 → 「部门/我的」页签解锁;
   「我的」内分「随身 / 本项目」两组;不填身份点这两个页签 → 提示先填身份。
   换一个别人的 wecom id → 「我的」页签看不到你的个人记忆（分域隔离目检）。
5. **搜索**：搜索框按当前页签检索（混合语义+关键词），行上带分数;「× 清除」回列表。
6. **admin 面**：`#/admin` 进 workspace 详情 → 「团队记忆」区块可按
   最近编辑/热度排序、按 kind 筛选、删除（个人/部门域不出现在 admin 面）。

## 二十二、清理

```bash
for d in /docs /a /rename-test /append-test /lines-test /range-test /cas-test \
         /dedup-test /edge /sql-test /tf /ev /search-test /large-test /fuse-dir \
         "/重命名目录" "/📁folder"; do
  veda rm "$d"
done
veda sql "SELECT COUNT(*) FROM files"
```

---

## 命令速查表

| 操作 | CLI | HTTP |
|------|-----|------|
| 初始化（账号+workspace+key） | `veda init` | `POST /v1/accounts` 等 |
| Workspace profile | `veda workspace add/switch/list/rm` | `POST /v1/workspaces`、`POST /v1/workspaces/{id}/keys` |
| 状态 | `veda status` | `GET /healthz`、`GET /v1/ready` |
| 写入文件 | `veda cp <src> <dst>` | `PUT /v1/fs/{path}` |
| 读取文件 | `veda cat <path>` | `GET /v1/fs/{path}` |
| 删除文件 | `veda rm <path>` | `DELETE /v1/fs/{path}` |
| 行号读取 | `veda cat <path> --range 1:10`（或 `--head/--tail N`） | `GET /v1/fs/{path}?lines=1:10` |
| 列出目录 | `veda ls <path>` | `GET /v1/fs/{path}?list` |
| 创建目录 | `veda mkdir <path>` | `POST /v1/fs-mkdir` |
| 重命名 | `veda mv <src> <dst>` | `POST /v1/fs-rename` |
| 复制 | ❌ 需 curl | `POST /v1/fs-copy` |
| 追加 | `veda append <path> <content>` | `POST /v1/fs/{path}` |
| Stat | ❌ 需 curl | `GET /v1/fs/{path}?stat=1` |
| Range 读取 | ❌ 需 curl | `GET /v1/fs/{path}` + `Range:` header |
| If-Match | ❌ 需 curl | `PUT /v1/fs/{path}` + `If-Match:` header |
| 搜索 | `veda search <query> [--path /p]` | `POST /v1/search`（`path_prefix`） |
| Grep | `veda grep <pattern> [path]` | `POST /v1/grep` |
| Collection | `veda collection *` | `POST/GET/DELETE /v1/collections` |
| SQL | `veda sql <query>` | `POST /v1/sql`（body `{"sql":"…"}`） |
| FUSE 挂载 | `veda-fuse mount [--server URL] [--key wk_…] <mountpoint>` | - |
