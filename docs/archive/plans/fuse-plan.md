# FUSE 增强计划

> 目标：补全 veda-fuse，使其达到 drive9 / fs9 级别的可用性。
> 原则：不过度设计，不考虑后向兼容。

---

## 现状

veda-fuse 已实现基本 FUSE 操作（lookup/getattr/readdir/read/write/create/mkdir/unlink/rmdir/rename/setattr），但存在以下问题：

1. 无后台挂载 — `fuser::mount2` 阻塞主线程，`--foreground false` 实际没有 daemon 化
2. 无 umount 命令 — 只能手动 `umount` 或依赖 `AutoUnmount`
3. 无远程变更通知 — 其他客户端修改文件后，FUSE 端不感知，靠 5s attr TTL 被动刷新
4. 每次 read 下载整个文件 — 大文件性能灾难
5. 无读缓存 — 同一文件反复读取每次都发 HTTP
6. flush + release 双写 — 同一个文件可能被 PUT 两次
7. mount 选项太少 — 不支持 cache-size / TTL 配置 / read-only 等

---

## Step 1: 后台挂载 + umount 命令

**目标**：`veda-fuse mount` 默认后台运行，`veda-fuse umount` 优雅卸载。

### 改动

**main.rs** — 重构为子命令：

```
veda-fuse mount --server X --key Y /mnt/veda     # 默认后台
veda-fuse mount --foreground /mnt/veda            # 前台（调试用）
veda-fuse umount /mnt/veda                        # 卸载
```

**daemon 化**：
- 使用 `nix::unistd::fork()` + `setsid()` 实现 daemon
- daemon 进程先 health check，成功后 parent 退出（用 pipe 通知）
- `--foreground` 跳过 fork，直接 mount（调试用）

**umount**：
- macOS: `umount <mountpoint>`
- Linux: `fusermount3 -u <mountpoint>` 或 `fusermount -u` 或 `umount`

**信号处理**：
- 捕获 SIGINT/SIGTERM，优雅 flush 所有写缓冲后 unmount

### 测试

- 单元测试：`umount_argv()` 按 OS 选择正确命令
- 集成测试：mount → ls → umount 全流程（需要 macFUSE/libfuse）

---

## Step 2: SSE 事件推送

**目标**：server 端推送文件变更事件，FUSE 端实时失效缓存。

### Server 端

**新路由** `GET /v1/events`：
- SSE (Server-Sent Events)，比 WebSocket 简单，单向推送够用
- 查询参数：`since_id`（上次游标）、`actor`（自身 actor ID，用于过滤自己的变更）
- 轮询 `veda_fs_events` 表，每秒一次
- 事件格式：`data: {"id":123,"event_type":"update","path":"/docs/a.txt"}\n\n`
- 无新事件时发 heartbeat（30s 间隔）

### FUSE 端

**SseWatcher**：
- mount 时生成随机 actor_id，server 端 API 要能识别
- 后台线程连接 SSE，收到事件后：
  - 跳过自身 actor 产生的事件
  - 失效 InodeTable 中对应 path 的 attr cache
  - 失效 read cache（Step 3 实现后）
  - 失效父目录 attr cache（触发 readdir 刷新）
- 断线自动重连（指数退避，1s → 30s）

### 改动

- `veda-server/src/routes/events.rs` — 新 SSE 端点
- `veda-fuse/src/sse.rs` — SSE watcher
- `veda-fuse/src/fs.rs` — InodeTable 改为 `Arc<Mutex<>>` 以支持跨线程失效

### 测试

- 单元测试：SSE 事件解析、actor 过滤
- 集成测试：client A 写文件 → SSE 推送 → client B 收到事件

---

## Step 3: 读优化

**目标**：支持 Range 读 + LRU 读缓存，大文件不再每次全量下载。

### Server 端

- `read_file` handler 支持 HTTP `Range` header
- 返回 `206 Partial Content` + `Content-Range` header
- 对 inline 文件（≤256KB）直接切片返回
- 对 chunked 文件按 chunk_index 定位，只读取涉及的 chunks

### FUSE client 端

**VedaClient**：
- 新增 `read_range(path, offset, length) -> Vec<u8>` 方法
- 发送 `Range: bytes=offset-end` header

**ReadCache (LRU)**：
- 简单的 `HashMap<String, (Vec<u8>, Instant)>` + 容量上限
- 仅缓存小文件（≤ cache entry 上限，默认 1MB）
- TTL 过期 + SSE 事件主动失效
- 大文件走 range read，不缓存

**FUSE read 逻辑**：
- 小文件（≤1MB）：优先 read cache，miss 时全量下载并缓存
- 大文件（>1MB）：直接 range read，不缓存

### 测试

- 单元测试：LRU cache 容量淘汰、TTL 过期、invalidate
- 集成测试：range read 正确返回部分内容

---

## Step 4: 写优化

**目标**：消除 flush+release 双写，提升写入可靠性。

### 改动

- `flush`：执行 PUT，标记 buffer 为 `flushed = true`
- `release`：仅当 `flushed == false` 时 PUT，然后移除 buffer
- 数据结构从 `HashMap<u64, (u64, Vec<u8>)>` 改为 `HashMap<u64, WriteHandle>`：

```rust
struct WriteHandle {
    ino: u64,
    buf: Vec<u8>,
    flushed: bool,
    dirty_since_flush: bool,  // flush 后有新 write 则需要再 PUT
}
```

### 测试

- 单元测试：WriteHandle 状态机（open → write → flush → release 只 PUT 一次）
- 单元测试：flush 后再 write → release 时需要再 PUT

---

## Step 5: Mount 选项

**目标**：提供常用 mount 配置。

### 新增参数

| 参数 | 默认值 | 说明 |
|------|--------|------|
| `--cache-size` | 128 | 读缓存大小 (MB) |
| `--attr-ttl` | 5 | attr 缓存 TTL (秒) |
| `--dir-ttl` | 5 | 目录缓存 TTL (秒) |
| `--allow-other` | false | 允许其他用户访问挂载点 |
| `--read-only` | false | 只读挂载 |
| `--debug` | false | FUSE debug 日志 |

### 改动

- `main.rs` — clap 参数扩展
- `fs.rs` — 使用配置中的 TTL 值替代硬编码 5s
- read-only 模式：open/write/create/mkdir/unlink/rmdir/rename 返回 EROFS

### 测试

- 单元测试：read-only 模式下写操作返回 EROFS

---

## 实施顺序

Step 1 → Step 4 → Step 5 → Step 3 → Step 2

理由：
- Step 1（daemon + umount）是日常使用的基础
- Step 4（修复双写）是 trivial fix，顺手做
- Step 5（mount 选项）简单，依赖少
- Step 3（range read + cache）需要 server 端改动
- Step 2（SSE）依赖 Step 3 的 cache 失效机制，放最后
