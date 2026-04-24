# 文件覆盖场景一致性方案（Phase 1）

> 作者：Claude + Joe
> 日期：2026-04-22
> 状态：待 review
> 关联：`docs/fuse-plan.md`、`crates/veda-core/src/service/fs.rs`、`crates/veda-fuse/src/fs.rs`

---

## 0. TL;DR

当前 veda 的文件覆盖（`PUT /v1/fs`、`POST /v1/fs` append、FUSE `flush`）在**并发写同一路径**和**FUSE open→flush 期间远端被改**两种场景下会静默丢失数据，chunked 存储还会出现 chunk 撕裂（A 的 chunk 0 + B 的 chunk 1 拼接）。

本期（Phase 1）做 **6 个正交改动**关掉上述问题，保持 HTTP API 向前兼容：

1. `update_file_revision` 改成 **CAS**（`WHERE revision=?`）
2. service 层入口加 `**SELECT ... FOR UPDATE`** 串行化并发写
3. COW 分支补 **ref_count→0 孤儿清理**
4. tx 外套 **deadlock retry** wrapper
5. HTTP 层支持 `**If-Match` / `ETag`**（语义：last-writer-wins 向前兼容，带 `If-Match` 时启用乐观并发）
6. FUSE 侧 **记录 BaseRev**，flush 带 `If-Match`，冲突 → `EBUSY`

参考实现：**[mem9-ai/drive9](https://github.com/mem9-ai/drive9)** 的 `pkg/fuse/{write,writeback,handle,commit_queue}.go` 和 `clients/drive9-rs/src/patch.rs`。其 sparse part buffer + PATCH 协议、shadow + WAL + CommitQueue 三项高级设计放到 Phase 2 / Phase 3，本期不做。

---

## 1. 背景

### 1.1 当前覆盖路径

```
HTTP:  PUT/POST /v1/fs/{*path}   (routes/fs.rs:71-91, 93-113)
         │
Service: FsService::write_file / append_file / copy_file
         (core/src/service/fs.rs:29-953)
         │
Tx:     MetadataStore::begin_tx                         ─┐
          SELECT dentry                                   │
          SELECT file                                     │  MySQL
          DELETE file_contents / file_chunks              │  default
          INSERT / UPDATE file_contents / file_chunks     │  REPEATABLE READ
          UPDATE files SET revision=revision+1, ...       │  无显式锁
          INSERT outbox, fs_event                         │
         commit                                          ─┘
         │
FUSE:   VedaFs::flush_handle
          open(): 整文件读到 WriteHandle.buf (fs.rs:299-320)
          write()/read(): 落内存缓冲
          flush(): client.write_file(path, whole_buffer)   ← 整体覆盖
```

### 1.2 问题清单


| #   | 场景                       | 严重度     | 表现                                                                                                                                                               |
| --- | ------------------------ | ------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| P1  | 并发 `PUT` 同一路径            | 🔴 数据损坏 | 两个 tx 都读到 `revision=3`、都 `UPDATE revision=4`；**chunked 分支**下 `DELETE file_chunks + N 条 INSERT` 非原子，交错后产生 A/B 混合内容                                                |
| P2  | 并发 `POST` append         | 🔴 数据丢失 | read-modify-write，二者读到相同 base，后提交者覆盖前者的追加                                                                                                                        |
| P3  | FUSE open→flush 期间远端被改   | 🔴 数据丢失 | `open(O_RDWR)` 读快照到 buf；flush 回写整份内容，服务端无 `If-Match`，直接覆盖                                                                                                        |
| P4  | 双 COW                    | 🟠 资源泄漏 | `ref_count=2` 同时被两个 path 触发 COW，各自 `decrement 2→1→0`，都没观察到归零，旧 file 行 + content/chunks **永久孤立**；`write_file:91` 和 `copy_file:647` 的 COW 没复用 `delete:515-523` 的清理 |
| P5  | 小改动整盘重写                  | 🟡 效率   | 单引用分支 `DELETE file_chunks` + 重 INSERT 全部 chunk；40MB 文件改 1B 重写 160 块                                                                                              |
| P6  | SSE 1s 轮询                | 🟡 延迟   | `events.rs:15 POLL_INTERVAL=1s`；写提交→订阅方失效可见窗口 ≥1s                                                                                                                |
| P7  | 并发新建同一 path              | 🟡 UX   | 后提交者 `insert_dentry` 命中 UNIQUE → `AlreadyExists` 报错，语义上应该等同于覆盖                                                                                                   |
| P8  | 无死锁重试                    | 🟡 健壮性  | MySQL `ER_LOCK_DEADLOCK(1213)` / `ER_LOCK_WAIT_TIMEOUT(1205)` 直接抛给用户                                                                                             |
| P9  | `delete_file_content` 冗余 | 🟢 微优   | `mysql.rs:1152` 已 `ON DUPLICATE KEY UPDATE`，前面的 DELETE 是多余的 round trip                                                                                           |
| P10 | 无 ETag/If-Match          | 🟢 能力缺失 | `WriteFileResponse.revision` 已存在但未绑定 HTTP 头，客户端无法做乐观并发控制                                                                                                         |


---

## 2. drive9 的参考设计

读过的关键代码（commit 时 `HEAD` of main）：

- `pkg/fuse/write.go` — `WriteBuffer`：8MB 稀疏 part map、per-part dirty bit、lazy LoadPart、sequential 检测、part eviction
- `pkg/fuse/writeback.go` — `WriteBackCache`：本地 `.dat + .meta` 持久化、`PutWithBaseRev(path, data, size, kind, baseRev)`、`PendingKind ∈ {New, Overwrite, Conflict}`
- `pkg/fuse/handle.go` — `FileHandle.BaseRev`、`OrigSize`、`ShadowReady`
- `pkg/fuse/commit_queue.go` — 后台 worker pool、`inFlight[path]` per-path 串行、`WaitPath/WaitPrefix` 阻塞语义、`CancelPath/CancelPrefix` 防止 "unlink 后被背景复活"、`ErrConflict → onCommitTerminalFailure` 标 `PendingConflict` **保留本地数据**
- `pkg/fuse/shadow.go` — `ShadowFile.{fd, size, baseRev, extents}`、pread/pwrite、`CheckDiskSpace`
- `pkg/fuse/journal.go` — append-only WAL，frame `[len:u32][payload:N][crc32:u32]`，`Replay/Compact` 恢复
- `clients/drive9-rs/src/patch.rs` — PATCH 协议：body `{new_size, dirty_parts, expected_revision, part_size}`，服务端返回 `PatchPlan { upload_parts[], copied_parts[], upload_id }`，未变 part 服务端 `UploadPartCopy` 零传输

三层设计对照 veda：


| drive9 层                          | 功能                                    | veda 现状        | 本期做不做     |
| --------------------------------- | ------------------------------------- | -------------- | --------- |
| `BaseRev + expected_revision` CAS | 开 handle 时抓 revision，flush 带 If-Match | ❌ 无            | ✅ **本期做** |
| Sparse part buffer + PATCH        | 只传 dirty parts，服务端复用                  | ❌ 整文件 buffer   | Phase 2   |
| Shadow + WAL + CommitQueue        | 本地持久化 + 后台异步上传 + crash recover        | ❌ flush 同步，失败丢 | Phase 3   |


---

## 3. 本期方案详细设计

### 3.1 改动 1 — CAS `update_file_revision`

**文件**：`crates/veda-store/src/mysql.rs:1019-1043`、`crates/veda-core/src/store.rs`（trait）

**修改**：把 trait 方法签名改成 CAS。

```rust
// trait MetadataTx (crates/veda-core/src/store.rs)
// Before:
async fn update_file_revision(
    &mut self, file_id: &str, revision: i32,
    size_bytes: i64, checksum: &str,
    line_count: Option<i32>, storage_type: StorageType,
) -> Result<()>;

// After:
async fn cas_update_file_revision(
    &mut self, file_id: &str,
    expected_rev: i32, new_rev: i32,
    size_bytes: i64, checksum: &str,
    line_count: Option<i32>, storage_type: StorageType,
) -> Result<bool>;  // true = updated; false = rev mismatch
```

MySQL 实现：

```rust
// crates/veda-store/src/mysql.rs
async fn cas_update_file_revision(
    &mut self, file_id: &str, expected_rev: i32, new_rev: i32,
    size_bytes: i64, checksum: &str, line_count: Option<i32>,
    storage_type: StorageType,
) -> Result<bool> {
    let t = self.tx_mut()?;
    let r = sqlx::query(
        r#"UPDATE veda_files
             SET revision = ?, size_bytes = ?, checksum_sha256 = ?,
                 line_count = ?, storage_type = ?
           WHERE id = ? AND revision = ?"#,
    )
    .bind(new_rev).bind(size_bytes).bind(checksum)
    .bind(line_count).bind(storage_type_str(storage_type))
    .bind(file_id).bind(expected_rev)
    .execute(t.as_mut())
    .await
    .map_err(storage_err)?;
    Ok(r.rows_affected() == 1)
}
```

**为什么独立做这一层**：即便有行锁（改动 2），CAS 也是**纵深防御** —— 万一未来调整锁策略、或某条代码路径忘记拿锁，revision 列的 CAS 会让冲突直接暴露为错误而不是静默损坏。

### 3.2 改动 2 — `SELECT ... FOR UPDATE`

**文件**：`crates/veda-core/src/store.rs`（trait）、`crates/veda-store/src/mysql.rs`、`crates/veda-core/src/service/fs.rs`

**新增 trait 方法**（MetadataTx）：

```rust
async fn get_dentry_for_update(&mut self, workspace_id: &str, path: &str) -> Result<Option<Dentry>>;
async fn get_file_for_update(&mut self, file_id: &str) -> Result<Option<FileRecord>>;
```

MySQL 实现复用现有 `get_dentry_conn` / `get_file_conn`，SQL 末尾加 `FOR UPDATE`：

```sql
SELECT ... FROM veda_dentries WHERE workspace_id = ? AND path = ? FOR UPDATE
SELECT ... FROM veda_files    WHERE id = ?                        FOR UPDATE
```

**调用点**：`FsService::write_file` / `append_file` / `copy_file` / `delete` / `rename` 开 tx 后第一次读 dentry/file 全部换成 `_for_update` 版本。

**锁顺序**：一律先 dentry 后 file，避免 A/B 互相等对方的死锁。

> ⚠️ 注意：`get_dentry` 按 `(workspace_id, path)` 查，依赖 `(workspace_id, path)` 上的索引，`FOR UPDATE` 会锁住命中的 index record + gap。gap 锁可能在并发新建时互相阻塞；配合改动 4（deadlock retry）兜底。

### 3.3 改动 3 — COW 孤儿清理

**文件**：`crates/veda-core/src/service/fs.rs`

抽一个 helper：

```rust
async fn cleanup_file_if_orphan(
    tx: &mut dyn MetadataTx,
    workspace_id: &str,
    file_id: &str,
) -> Result<()> {
    let remaining = tx.decrement_ref_count(file_id).await?;
    if remaining <= 0 {
        tx.delete_file_content(file_id).await?;
        tx.delete_file_chunks(file_id).await?;
        let outbox = make_outbox(workspace_id, OutboxEventType::ChunkDelete, file_id);
        tx.insert_outbox(&outbox).await?;
        tx.delete_file(file_id).await?;
    }
    Ok(())
}
```

**替换点**：

- `fs.rs:91` (`write_file` 的 COW 分支) —— 当前只有 `decrement_ref_count(fid)`，替换为 `cleanup_file_if_orphan`
- `fs.rs:779` (`append_file` 的 COW 分支) —— 同上
- `fs.rs:647` (`copy_file` 覆盖目标时的引用减少) —— 原本已有类似代码，重构成调 helper
- `fs.rs:515-523` (`delete`) —— 可选重构成调 helper

### 3.4 改动 4 — Deadlock retry wrapper

**文件**：`crates/veda-core/src/service/mod.rs` 新增 helper；`crates/veda-core/src/service/fs.rs` 写类方法套一层

```rust
// crates/veda-core/src/service/mod.rs
const MAX_TX_RETRIES: u32 = 3;

pub async fn retry_on_deadlock<F, Fut, T>(mut op: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let mut attempt = 0u32;
    loop {
        match op().await {
            Err(VedaError::Storage(msg))
                if is_retryable_deadlock(&msg) && attempt < MAX_TX_RETRIES =>
            {
                attempt += 1;
                let backoff_ms = 10 * (1 << attempt); // 20, 40, 80
                tokio::time::sleep(
                    std::time::Duration::from_millis(backoff_ms)
                ).await;
                continue;
            }
            other => return other,
        }
    }
}

fn is_retryable_deadlock(msg: &str) -> bool {
    // MySQL error 1213 = ER_LOCK_DEADLOCK, 1205 = ER_LOCK_WAIT_TIMEOUT
    msg.contains("1213") || msg.contains("Deadlock")
        || msg.contains("1205") || msg.contains("Lock wait timeout")
}
```

> ⚠️ 字符串匹配不是最优；长期方案是在 `crates/veda-store/src/mysql.rs::storage_err` 把 sqlx 错误映射成结构化 `VedaError::RetryableStorage { kind: Deadlock | LockTimeout, .. }`。本期先用字符串，记一个 TODO。

**调用示例**：

```rust
// crates/veda-core/src/service/fs.rs
pub async fn write_file(&self, ws: &str, path: &str, content: &str) -> Result<WriteFileResponse> {
    retry_on_deadlock(|| self.write_file_once(ws, path, content)).await
}
async fn write_file_once(&self, ws: &str, path: &str, content: &str)
    -> Result<WriteFileResponse>
{
    // 原 write_file 逻辑挪到这里
}
```

### 3.5 改动 5 — HTTP `If-Match` / `ETag`

**文件**：`crates/veda-server/src/routes/fs.rs:71-91, 93-113`、`crates/veda-server/src/error.rs`、`crates/veda-types/src/errors.rs`、`crates/veda-core/src/service/fs.rs`

新增错误码：

```rust
// crates/veda-types/src/errors.rs
#[derive(thiserror::Error, Debug)]
pub enum VedaError {
    // ... existing ...
    #[error("precondition failed: {0}")]
    PreconditionFailed(String),
}
```

错误映射：

```rust
// crates/veda-server/src/error.rs
impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, body) = match self.0 {
            // ...
            VedaError::PreconditionFailed(_) => (StatusCode::PRECONDITION_FAILED, ...),
        };
        // ...
    }
}
```

Service 签名：

```rust
// crates/veda-core/src/service/fs.rs
pub async fn write_file(
    &self,
    workspace_id: &str,
    raw_path: &str,
    content: &str,
    expected_revision: Option<i32>,    // ← 新增
) -> Result<api::WriteFileResponse> {
    // 拿到 existing file 后:
    if let Some(expected) = expected_revision {
        if let Some(ref f) = existing_file {
            if f.revision != expected {
                return Err(VedaError::PreconditionFailed(format!(
                    "revision mismatch: expected {}, actual {}",
                    expected, f.revision
                )));
            }
        } else if expected != 0 {
            // 客户端声称 revision=N，但文件不存在 → 也算不匹配
            return Err(VedaError::PreconditionFailed(format!(
                "file not found; expected revision {}", expected
            )));
        }
    }
    // ... 其余不变
}
```

HTTP handler：

```rust
// crates/veda-server/src/routes/fs.rs
async fn write_file(
    State(state): State<Arc<AppState>>,
    auth: AuthWorkspace,
    Path(path): Path<String>,
    headers: HeaderMap,                           // ← 新增
    body: Result<String, StringRejection>,
) -> Result<Response, AppError> {
    if auth.read_only { return Err(...); }
    let body = body.map_err(...)?;
    let path = format!("/{path}");

    let expected_rev = parse_if_match(&headers);   // ← 新增

    let resp = state.fs_service
        .write_file(&auth.workspace_id, &path, &body, expected_rev)
        .await?;

    let mut r = Json(ApiResponse::ok(resp.clone())).into_response();
    r.headers_mut().insert(
        header::ETAG,
        HeaderValue::from_str(&format!("\"{}\"", resp.revision)).unwrap(),
    );
    Ok(r)
}

fn parse_if_match(h: &HeaderMap) -> Option<i32> {
    let v = h.get(header::IF_MATCH)?.to_str().ok()?;
    let v = v.trim().trim_matches('"');
    v.parse().ok()
}
```

**向前兼容**：`If-Match` 缺省时 `expected_revision = None`，走 last-writer-wins（但有改动 1+2 保护不撕裂）。旧 `veda-cli`、SQL UDF、`curl` 脚本零改动可用。

### 3.6 改动 6 — FUSE `WriteHandle.base_rev`

**文件**：`crates/veda-fuse/src/fs.rs:35-39, 299-320, 162-183`、`crates/veda-fuse/src/client.rs`

```rust
// crates/veda-fuse/src/fs.rs
struct WriteHandle {
    ino: u64,
    buf: Vec<u8>,
    dirty: bool,
    base_rev: Option<i32>,    // ← 新增；open 时 stat 填充；new file = None
}
```

`open(writable)` (`fs.rs:299-320`)：读内容那一步附带记 `base_rev`。`stat` 返回的 `FileInfo.revision` 已存在，client 侧已解析。

```rust
fn open(&mut self, _req: &Request, ino: u64, flags: i32, reply: ReplyOpen) {
    // ... existing ...
    let (existing_buf, base_rev) = if (flags & libc::O_TRUNC) != 0 {
        (Vec::new(), None)
    } else {
        match self.client.stat_and_read(&path) {
            Ok((info, content)) => (content.into_bytes(), info.revision),
            Err(ClientError::NotFound) => (Vec::new(), None),
            Err(ref e) => { reply.error(Self::err_to_errno(e)); return; }
        }
    };
    self.write_handles.insert(fh, WriteHandle { ino, buf: existing_buf, dirty: false, base_rev });
    reply.opened(fh, 0);
}
```

> 可以把 `stat + read` 合并成一次 HTTP 调用减少 RTT；或者保留两次但共享 ino 缓存。

`flush_handle` (`fs.rs:162-183`)：

```rust
fn flush_handle(&mut self, fh: u64, ino: u64) -> Result<(), i32> {
    let is_dirty = self.write_handles.get(&fh).map_or(false, |h| h.dirty);
    if !is_dirty { return Ok(()); }
    let path = self.inode_get_path(ino).ok_or(libc::ENOENT)?;
    let (buf, base_rev) = {
        let h = self.write_handles.get(&fh).unwrap();
        (h.buf.clone(), h.base_rev)
    };
    match self.client.write_file(&path, &buf, base_rev) {
        Ok(new_rev) => {
            if let Some(h) = self.write_handles.get_mut(&fh) {
                h.dirty = false;
                h.base_rev = Some(new_rev);      // ← 同一 handle 继续写会沿用新 rev
            }
            self.inode_invalidate(ino);
            self.cache_invalidate(&path);
            Ok(())
        }
        Err(ClientError::Conflict) => {
            warn!(path = %path, "flush conflict: remote advanced since open");
            Err(libc::EBUSY)                     // ← 用户看到的错误码
        }
        Err(ref e) => {
            warn!(path = %path, err = %e, "flush failed");
            Err(Self::err_to_errno(e))
        }
    }
}
```

`VedaClient::write_file` 签名（`crates/veda-fuse/src/client.rs`）：

```rust
pub enum ClientError {
    // ... existing ...
    Conflict,                                    // ← 新增
}

pub fn write_file(
    &self,
    path: &str,
    body: &[u8],
    expected_rev: Option<i32>,                   // ← 新增
) -> Result<i32, ClientError> {                  // ← 返回新 revision
    let mut req = self.http.put(self.fs_url(path)).bearer_auth(&self.key).body(body.to_vec());
    if let Some(rev) = expected_rev {
        req = req.header(reqwest::header::IF_MATCH, format!("\"{rev}\""));
    }
    let resp = req.send()?;
    if resp.status() == 412 { return Err(ClientError::Conflict); }
    // ...
    let rev = parse_etag(&resp).ok_or(ClientError::Io(...))?;
    Ok(rev)
}
```

**用户侧可见行为**：

```
# terminal A: vim mnt/doc.txt (opened at revision=5)
# terminal B: curl -X PUT $SRV/v1/fs/doc.txt -d "new"   → bumps to revision=6
# terminal A: :wq                                         → flush with If-Match=5
                                                         → 412 → EBUSY
                                                         → vim 报 "E667: Fsync failed"
# terminal A: :!cat mnt/doc.txt  → 看到 terminal B 的内容，决定 merge 后再写
```

对比现状：**现状是 terminal B 的写入被 terminal A 的 flush 静默覆盖，没有任何提示**。

### 3.7 改动 7 — 杂项

**P7 修复**：`fs.rs:174-191` 新建分支遇到 `insert_dentry` 返回 `AlreadyExists` 时，**回滚并重跑一次**（此时对方已提交，我们走"有 existing dentry"分支）。实现上最简单：直接抛一个 retriable 错误让 `retry_on_deadlock` 重跑。

**P9 清理**：`fs.rs:119` 和 `fs.rs:803` 的 `tx.delete_file_content(fid)` 删除，因为 `insert_file_content`（`mysql.rs:1152`）已是 `ON DUPLICATE KEY UPDATE`。注意 **chunked→inline 切换**时还需要 `delete_file_chunks`，这个保留。

---

## 4. 冲突语义真值表


| 客户端带 `If-Match`? | 服务端 `revision` 现值 | 客户端值 | 结果                                                                                           |
| ---------------- | ----------------- | ---- | -------------------------------------------------------------------------------------------- |
| ❌ 无              | 任意                | —    | **写成功**；行锁保证不撕裂；返回新 `ETag`                                                                   |
| ✅ 有              | 3                 | 3    | 写成功；返回 `ETag: "4"`                                                                           |
| ✅ 有              | 4                 | 3    | **412 Precondition Failed**；服务端不变更                                                           |
| ✅ 有              | 文件不存在             | 0    | 等同于新建语义，写成功（预留：用 `0` 表示"我预期文件不存在"）                                                           |
| ✅ 有              | 文件不存在             | 3    | **412**；服务端不创建                                                                               |
| —                | 同一路径并发写（A/B）      | —    | A 先拿到 `FOR UPDATE` 行锁；B 阻塞；A commit 后 B 看到新 revision；如 B 带 `If-Match` 且值过期 → B 得 412；否则 B 成功 |
| —                | 同一路径并发 COW        | —    | 行锁串行；`cleanup_file_if_orphan` 在 ref_count=0 时清理旧 file                                        |


**FUSE 层映射**：


| HTTP     | FUSE errno    |
| -------- | ------------- |
| 200      | `ok`          |
| 412      | `libc::EBUSY` |
| 404      | `ENOENT`      |
| 413      | `EFBIG`       |
| 403      | `EACCES`      |
| 5xx / 网络 | `EIO`         |


---

## 5. 测试策略

### 5.1 单元测试

**文件**：`crates/veda-core/tests/fs_service_test.rs`

```rust
#[tokio::test]
async fn concurrent_overwrite_no_corruption() {
    let (svc, state) = make_service();
    svc.write_file("ws1", "/f.txt", "v0", None).await.unwrap();
    let svc = Arc::new(svc);

    let svc1 = svc.clone();
    let svc2 = svc.clone();
    let h1 = tokio::spawn(async move {
        svc1.write_file("ws1", "/f.txt", &"A".repeat(500_000), None).await
    });
    let h2 = tokio::spawn(async move {
        svc2.write_file("ws1", "/f.txt", &"B".repeat(500_000), None).await
    });
    let (r1, r2) = tokio::join!(h1, h2);
    r1.unwrap().unwrap();
    r2.unwrap().unwrap();

    let final_content = svc.read_file("ws1", "/f.txt").await.unwrap();
    // 断言最终内容是全 A 或全 B，不是混合
    assert!(
        final_content.chars().all(|c| c == 'A')
            || final_content.chars().all(|c| c == 'B'),
        "corrupted mix detected"
    );
    let st = state.lock().unwrap();
    let f = st.files.iter().find(|f| f.id != "").unwrap();
    assert_eq!(f.revision, 3); // v0=1, A=2, B=3
}

#[tokio::test]
async fn concurrent_append_no_lost_update() {
    // 两并发 append "X" 和 "Y" 到 "base"，最终 size = 5；内容包含 XY 或 YX
}

#[tokio::test]
async fn if_match_mismatch_returns_precondition_failed() {
    let (svc, _) = make_service();
    let r1 = svc.write_file("ws1", "/f.txt", "v1", None).await.unwrap();
    svc.write_file("ws1", "/f.txt", "v2", None).await.unwrap();
    let err = svc.write_file("ws1", "/f.txt", "v3", Some(r1.revision)).await;
    assert!(matches!(err, Err(VedaError::PreconditionFailed(_))));
}

#[tokio::test]
async fn double_cow_cleans_orphan() {
    let (svc, state) = make_service();
    svc.write_file("ws1", "/a.txt", "shared", None).await.unwrap();
    svc.copy_file("ws1", "/a.txt", "/b.txt").await.unwrap();
    // 并发 COW
    let svc = Arc::new(svc);
    let s1 = svc.clone(); let s2 = svc.clone();
    tokio::join!(
        async move { s1.write_file("ws1", "/a.txt", "A", None).await.unwrap() },
        async move { s2.write_file("ws1", "/b.txt", "B", None).await.unwrap() },
    );
    // 断言旧 file 行 + content 已删除
    let st = state.lock().unwrap();
    assert_eq!(st.files.iter().filter(|f| f.checksum_sha256 == sha256("shared")).count(), 0);
    assert_eq!(st.file_contents.iter().filter(|c| c.content == "shared").count(), 0);
}
```

### 5.2 集成测试

**文件**：`crates/veda-server/tests/server_test.rs`

```rust
#[tokio::test]
#[ignore]
async fn put_returns_etag_header() {
    let r = http_put("/v1/fs/x.txt", b"hello").await;
    assert_eq!(r.headers().get("ETag").unwrap(), "\"1\"");
}

#[tokio::test]
#[ignore]
async fn put_with_stale_if_match_returns_412() {
    http_put("/v1/fs/x.txt", b"v1").await;     // rev=1
    http_put("/v1/fs/x.txt", b"v2").await;     // rev=2
    let r = http_put_with_if_match("/v1/fs/x.txt", b"v3", 1).await;
    assert_eq!(r.status(), 412);
}

#[tokio::test]
#[ignore]
async fn put_with_fresh_if_match_succeeds() {
    let r1 = http_put("/v1/fs/x.txt", b"v1").await;
    let etag = r1.headers().get("ETag").unwrap().to_str().unwrap();
    let rev: i32 = etag.trim_matches('"').parse().unwrap();
    let r2 = http_put_with_if_match("/v1/fs/x.txt", b"v2", rev).await;
    assert_eq!(r2.status(), 200);
}
```

### 5.3 Deadlock 回归

启动 20 个 tokio task 并发 `write_file("ws1", "/hot.txt", random_content)`，断言：

- 全部返回 `Ok`（retry 兜底）
- 最终 `revision == 21`（含初始值 0）
- 日志中没有未捕获的 `ER_LOCK_DEADLOCK`

### 5.4 FUSE 手测

```bash
# Terminal 1: mount
drive9 … veda mount ~/mnt-veda

# Terminal 2: concurrent overwrite
echo "A" > ~/mnt-veda/x.txt
# 确认 stat 可看
# 在 vim 里打开同一文件，不要保存
vim ~/mnt-veda/x.txt   # 读到 "A"

# Terminal 3: remote overwrite through HTTP
curl -X PUT -H "Authorization: Bearer $KEY" \
     -d "B" "$SRV/v1/fs/x.txt"

# Terminal 2 (vim): make a change and :wq
# 预期: vim 报 "fsync failed / E667" 或类似；unix 错误码 EBUSY
# 预期: 再次 cat x.txt 得到 "B"（Terminal 3 的内容），不是 vim 的版本
```

补一个 shell 脚本 `e2e/fuse-overwrite-conflict.sh` 自动化这个场景。

---

## 6. 风险与回滚

### 6.1 风险


| 风险                                                    | 可能性 | 影响  | 缓解                                                        |
| ----------------------------------------------------- | --- | --- | --------------------------------------------------------- |
| `SELECT FOR UPDATE` 引入 gap 锁 → 并发新建热点路径卡顿             | 中   | 中   | deadlock retry 兜底；压测观察 P99                                |
| 死锁检测字符串匹配未来误判                                         | 低   | 低   | 记 TODO 改结构化错误                                             |
| FUSE `open` 多读一次 stat 使 open 延迟增加                     | 低   | 低   | stat 已经有 `InodeTable` attr 缓存，多数 open 走缓存；必要时合并 stat+read |
| 412 语义打破未显式声明 Semver 的 HTTP 客户端                       | 低   | 低   | 仅在 `If-Match` present 时返回 412；不带就是旧行为                     |
| 老 FUSE 客户端连新 server：FUSE 不发 If-Match，服务端不返 ETag → 无影响 | —   | —   | 向前兼容                                                      |
| 新 FUSE 客户端连老 server：服务端不识别 `If-Match`（直接忽略）、不返 `ETag` | 中   | 中   | FUSE 解析 ETag 失败时 fallback 到旧逻辑（`base_rev=None`），行为等同现状    |


### 6.2 回滚

改动相互独立，按改动号逆序 revert 即可。最小回滚单元：

- 回滚改动 5+6 → 关掉 If-Match，保留行锁和 CAS（仍比现状好）
- 回滚改动 2 → 保留 CAS 和 retry，放弃行锁（效率高但仅挡单引用分支的 lost-update，chunked 撕裂问题未解决）
- 回滚改动 4 → 去掉 retry，业务层可能看到偶发 deadlock 错误

不需要 DB migration：新增/修改全是 SQL 语句文本，schema 不变。

---

## 7. 不在本期范围（Phase 2 / Phase 3 展望）

### Phase 2：Part-based PATCH 协议

解决 P5（小改动整盘重写）和 P3 的效率放大：

- 服务端新增 `PATCH /v1/fs/{*path}`，body `{new_size, dirty_parts:[u32], part_size, expected_revision}`；服务端只对 `dirty_parts` 列出的 chunk_index 做 DELETE + INSERT。
- FUSE `WriteHandle.buf: Vec<u8>` 换成 `WriteBuffer`（对标 drive9 `pkg/fuse/write.go`），`parts: HashMap<u32, Vec<u8>>`、`dirty_parts: HashSet<u32>`、`part_size = 256KB`（复用 `INLINE_THRESHOLD`）、`LoadPart` lazy。
- FUSE `flush` 如果 `dirty_parts.len() * part_size < total_size * 0.8`，走 PATCH；否则仍走整体 PUT。

### Phase 3：本地持久化 + 异步上传

解决"flush 失败丢数据"和"foreground flush 阻塞"：

- `crates/veda-fuse/src/shadow.rs`：每 path 一个本地 shadow 文件，pread/pwrite
- `crates/veda-fuse/src/journal.rs`：append-only WAL，CRC32 帧
- `crates/veda-fuse/src/commit_queue.rs`：后台 worker pool、per-path 串行、cancellation 支持
- FUSE `flush` 只保证 shadow + journal 落盘就返回；后台上传失败标 `PendingConflict` 保留数据

---

## 8. 改动文件清单（一期）

### 新增/修改


| 文件                                                   | 改动性质      | 说明                                                                                                                          |
| ---------------------------------------------------- | --------- | --------------------------------------------------------------------------------------------------------------------------- |
| `crates/veda-types/src/errors.rs`                    | 新增        | `VedaError::PreconditionFailed(String)`                                                                                     |
| `crates/veda-core/src/store.rs`                      | 修改 trait  | `cas_update_file_revision`、`get_dentry_for_update`、`get_file_for_update`                                                    |
| `crates/veda-core/src/service/mod.rs`                | 新增 helper | `retry_on_deadlock`                                                                                                         |
| `crates/veda-core/src/service/fs.rs:29-953`          | 重构        | `write_file` / `append_file` / `copy_file` / `delete` 全部拿行锁 + CAS；抽 `cleanup_file_if_orphan` helper；签名增 `expected_revision` |
| `crates/veda-core/tests/fs_service_test.rs`          | 新增测试      | 见 §5.1                                                                                                                      |
| `crates/veda-core/tests/mock_store.rs`               | 修改 mock   | 实现新增 trait 方法；模拟行锁行为                                                                                                        |
| `crates/veda-store/src/mysql.rs`                     | 修改        | 新方法实现；保留向后兼容 `update_file_revision` 或直接删                                                                                    |
| `crates/veda-server/src/error.rs`                    | 修改        | `PreconditionFailed → 412`                                                                                                  |
| `crates/veda-server/src/routes/fs.rs:71-113`         | 修改        | handler 读 `If-Match`、返 `ETag`                                                                                               |
| `crates/veda-server/tests/server_test.rs`            | 新增测试      | 见 §5.2                                                                                                                      |
| `crates/veda-fuse/src/fs.rs:35-39, 162-183, 299-320` | 修改        | `WriteHandle.base_rev`、flush 带 If-Match、EBUSY 映射                                                                            |
| `crates/veda-fuse/src/client.rs`                     | 修改        | `write_file` 签名新增 `expected_rev`、返 revision；新增 `ClientError::Conflict`                                                      |
| `e2e/fuse-overwrite-conflict.sh`                     | 新建        | 见 §5.4                                                                                                                      |


### 不变

- `config/*.toml`
- DB schema（无 migration）
- CLI 命令（向前兼容）
- outbox / worker / SSE（和本次正交）

---

## 9. 术语表


| 术语                         | 含义                                                                |
| -------------------------- | ----------------------------------------------------------------- |
| **CAS**                    | Compare-And-Swap，基于 `WHERE revision=?` 的条件更新                      |
| **BaseRev**                | FUSE handle 在 open 时抓到的服务端 revision，flush 时作为 If-Match 传回         |
| **COW**                    | Copy-On-Write，file 的 `ref_count > 1` 时写入会 fork 出新 file_id 而非修改共享的 |
| **Gap lock**               | InnoDB 在 `SELECT ... FOR UPDATE` 命中索引时额外锁住 index 区间缝隙，防止幻读        |
| **PATCH**（Phase 2）         | 仅上传 dirty parts 的增量写协议，对比 `PUT` 整体覆盖                              |
| **Shadow**（Phase 3）        | 本地磁盘上的 pending 文件副本，pread/pwrite；flush 失败不丢数据                     |
| **WAL / Journal**（Phase 3） | 追加式日志，crash 后 replay 恢复                                           |


---

## 10. 参考

- [mem9-ai/drive9](https://github.com/mem9-ai/drive9) — `pkg/fuse/`
- drive9 `docs/async-embedding/async-embedding-phase-2-write-path.md` — "在同一 tx 里更新 revision + 清理旧 semantic state + 入队新 task" 的事务化设计原则（veda 当前用 outbox 模式已等价实现）
- MySQL 文档：[InnoDB Locking](https://dev.mysql.com/doc/refman/8.0/en/innodb-locking.html)、[Deadlocks](https://dev.mysql.com/doc/refman/8.0/en/innodb-deadlocks.html)
- RFC 7232 §3.1 — [If-Match](https://datatracker.ietf.org/doc/html/rfc7232#section-3.1)

