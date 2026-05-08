# Veda Alpha 试用指南

> 给愿意帮忙踩坑的工程师朋友。
> 大概 30 分钟能在一台干净 Linux 虚拟机上跑起来，挂载 FUSE，跑通 `make` / `rsync` / `git status`。
> 反馈欢迎直接在 GitHub 上发 issue ([github.com/jooekong/veda/issues](https://github.com/jooekong/veda/issues))，或者直接微信发我。

---

## 1. 试用什么

Veda 是一个把文件系统语义和向量检索绑在一起的服务：写进去的文件不仅能像普通 FS 一样读写、`ls`、`stat`，还会被自动切块 + 向量化，可以用 SQL 或 HTTP 跑语义搜索。这版 alpha 的目标形态是 **"用 FUSE 当工程师的工作目录，写代码时顺手可搜"**。

后端三件套：

- **MySQL** —— 文件元数据 + 目录树 + 事件流
- **Milvus** —— chunk 向量
- **veda-server** —— 把上面两个 stitch 起来，对外暴露 HTTP / SSE / SQL，内嵌 worker（embed） + reconciler（漂移修复） + retention（事件清理）

客户端：

- **veda CLI** —— `veda cp` / `veda ls` / `veda search`，单机调试用
- **veda-fuse** —— 把 workspace 挂成 mountpoint，`vim` / `make` / `git` 都能直接用

---

## 2. 准备工作

### 2.1 必备

- 一台 Linux 虚拟机（Ubuntu 22.04+ / Debian 12+），最少 4GB RAM、20GB 磁盘
- Docker 24+ 和 docker compose v2（`docker compose version` 能看到 v2.x）
- Rust 工具链 `rustup` —— 客户端二进制（`veda` CLI 和 `veda-fuse`）走 `cargo build` 编译
- 一份 OpenAI 兼容的 embedding API key（OpenAI / Azure / 阿里 dashscope / 自托管 vLLM 都行）
- macOS 试 FUSE 的话需要 `brew install macfuse`

### 2.2 可选

- LLM API key（用于摘要生成。不填的话 server 跳过 SummarySync 任务，搜索仍然能用）
- Grafana 看板偏好（compose 内置一个空 grafana，需要自己导面板）

---

## 3. 五步起来

### 3.1 拉代码

```bash
git clone https://github.com/jooekong/veda.git   # 或 git@github.com:jooekong/veda.git
cd veda/deploy
```

### 3.2 准备环境变量

```bash
cp .env.example .env
$EDITOR .env
```

至少填这三个（在 `.env.example` 第 4-14 行的 `# ── Required ──` 段）：

- `VEDA_JWT_SECRET` —— `openssl rand -hex 32` 生成
- `VEDA_METRICS_TOKEN` —— `openssl rand -hex 24` 生成
- `VEDA_EMBEDDING_API_KEY` —— 你的 embedding 提供商 key

### 3.3 准备 server config

```bash
cp config.docker.toml.example config.docker.toml
$EDITOR config.docker.toml
```

主要改 `[embedding]` section（也可以用 `VEDA_EMBEDDING_API_URL` / `VEDA_EMBEDDING_MODEL` / `VEDA_EMBEDDING_DIMENSION` 三个 env 覆盖；env 优先级高于 toml）：

```toml
# OpenAI
api_url = "https://api.openai.com/v1/embeddings"
model = "text-embedding-3-small"
dimension = 1536

# 阿里 dashscope (公网)
api_url = "https://dashscope.aliyuncs.com/compatible-mode/v1/embeddings"
model = "text-embedding-v4"
dimension = 1024
```

### 3.4 写入 metrics token 文件

Prometheus 用 `bearer_token_file` 读 token，所以 `.env` 里的值还得写一份到磁盘上：

```bash
mkdir -p secrets
( source .env && printf '%s' "$VEDA_METRICS_TOKEN" ) > secrets/veda_metrics_token
```

> ⚠️ 用 `printf '%s'` 不要用 `echo` —— `echo` 会带个尾换行，Prometheus 会把它当 token 的一部分发出去，scrape 立刻 401（实际是 404 — 见 §6 注意事项）。
> 用 `( source .env && ... )` 而不是 `grep | cut`，避免 base64 token 里 `=` 截断 / 引号漏处理。

### 3.5 起服务

```bash
docker compose up -d
docker compose ps
```

> ⚠️ **第一次 `up -d` 会触发本地 `cargo build --release` 来构建 veda-server 镜像，预计 10-20 分钟**（在普通 4 核 VM 上）。后续重启 60-120 秒。如果你想跳过构建，等我们 push 预编译镜像到 ghcr。

期望状态（`docker compose ps`）：

| Service | 期望状态 |
|---|---|
| `mysql` | `running (healthy)` |
| `etcd` | `running (healthy)` |
| `minio` | `running (healthy)` |
| `milvus` | `running (healthy)` |
| `veda-server` | `running` |
| `prometheus` | `running` |
| `grafana` | `running` |

验证：

```bash
# server 能响应（注意路径是 /v1/ready 不是 /healthz）
curl -s http://localhost:3000/v1/ready | jq

# metrics 可读
curl -s -H "Authorization: Bearer $(grep ^VEDA_METRICS_TOKEN .env | cut -d= -f2-)" \
     http://localhost:3000/v1/metrics | head -3

# Prometheus 看 veda-server target 是 UP
open http://localhost:9090/targets   # 或浏览器访问
```

#### 常见首次启动问题

- **`prometheus` 一直 `Exited (1)`**：八成是跳过了 §3.4。`docker compose logs prometheus` 看到 "no such file or directory" 就是这个原因（compose 启用了 `bind.create_host_path: false` 让缺文件直接报错而不是悄悄 mount 个空目录）。
- **`veda-server` 启动报 401 from embedding provider**：`docker compose logs veda-server` 找 `EmbeddingFailed`，常见是 `VEDA_EMBEDDING_API_KEY` 拼错或 `api_url` region 不对。
- **`milvus` 几分钟还 `unhealthy`**：cold start 可能 35-50s，`start_period=60s`。如果一直不 healthy，`docker compose logs milvus` 看是不是 etcd / minio 还没 ready。

---

## 4. 第一个 workspace + FUSE 挂载

CLI 已经把 onboarding 三件事（注册账号、建 workspace、生成 workspace key）封装好了，朋友直接用 CLI 就行。下面是 CLI 路径；如果你想用 curl，跳到 §4.5。

### 4.1 编译 CLI

```bash
cd ..   # 回到 repo root
cargo build --release --bin veda
```

可选地把 binary 放到 PATH：

```bash
sudo install -m 0755 target/release/veda /usr/local/bin/
```

下面假设 `veda` 在 PATH 里；否则写完整路径 `./target/release/veda`。

### 4.2 注册账号 → 建 workspace → 取 workspace key

CLI 默认 `--server http://localhost:3000`；override 用 `--server` 全局参数。

```bash
# 1. 注册一个账号。CLI 会把返回的 api_key 写入 ~/.config/veda/config.toml
veda account create \
    --name joe \
    --email joe@example.com \
    --password 'something-strong'

# 2. 在账号下建 workspace
veda workspace create --name demo
# → 返回 { "id": "<ws-uuid>", "name": "demo", ... }

# 3. 选用 workspace。这一步会调 POST /v1/workspaces/{id}/keys，
#    返回 workspace key 并写入本地 config（之后所有 fs 操作都用它）。
veda workspace use <ws-uuid>
```

### 4.3 用 CLI 写第一个文件

```bash
veda cp ./README.md /docs/README.md
veda ls /docs
veda cat /docs/README.md --lines 1:5
```

### 4.4 挂 FUSE

veda-fuse 在 workspace 之外（避免把 fuser dep 拉进 server 构建）。从仓库根：

```bash
cd crates/veda-fuse
cargo build --release
sudo install -m 0755 ../../target/release/veda-fuse /usr/local/bin/   # 可选
```

挂载（注意 flag 是 `--key`，不是 `--token`；mountpoint 是位置参数）：

```bash
sudo mkdir -p /mnt/veda-demo

# 取上一步 `veda workspace use` 写入 ~/.config/veda 的 ws_key
WS_KEY=$(grep ^ws_key ~/.config/veda/config.toml | sed 's/^ws_key *= *"\(.*\)"$/\1/')

veda-fuse mount \
    --server http://localhost:3000 \
    --key "$WS_KEY" \
    --foreground \
    /mnt/veda-demo
```

另起一个 terminal：

```bash
cd /mnt/veda-demo
ls -la docs           # 看到 README.md
cat docs/README.md
echo "hello" > docs/notes.txt
git init && git add . && git status
make -B               # mtime 应该正常
```

卸载：

```bash
veda-fuse umount /mnt/veda-demo   # 干净卸载
# 或回到 mount 进程的 terminal 按 Ctrl-C（--foreground 模式）
```

### 4.5 备选：纯 HTTP curl 路径

不想装 Rust 的话，全程也可以用 curl（没 CLI 的便利封装但能起来）：

```bash
SERVER=http://localhost:3000

# 1. 注册账号
ACCT_RESP=$(curl -s -X POST "$SERVER/v1/accounts" \
     -H "Content-Type: application/json" \
     -d '{"name":"joe","email":"joe@example.com","password":"something-strong"}')
ACCT_KEY=$(echo "$ACCT_RESP" | jq -r '.data.api_key')

# 2. 建 workspace（需要 account key）
WS_ID=$(curl -s -X POST "$SERVER/v1/workspaces" \
     -H "Authorization: Bearer $ACCT_KEY" \
     -H "Content-Type: application/json" \
     -d '{"name":"demo"}' | jq -r '.data.id')

# 3. 取 workspace key（用于后续 fs / search）
WS_KEY=$(curl -s -X POST "$SERVER/v1/workspaces/$WS_ID/keys" \
     -H "Authorization: Bearer $ACCT_KEY" \
     -H "Content-Type: application/json" \
     -d '{"name":"main"}' | jq -r '.data.api_key')

# 4. 写一个文件
curl -s -X PUT "$SERVER/v1/fs/docs/hello.md" \
     -H "Authorization: Bearer $WS_KEY" \
     -H "Content-Type: application/octet-stream" \
     --data-binary "Hello world"

# 5. 看一下
curl -s "$SERVER/v1/fs/docs" -H "Authorization: Bearer $WS_KEY" | jq
```

---

## 5. 跑一次搜索

```bash
veda search "如何启动" --mode semantic --limit 5
```

或者用 SQL（注意 body 是 JSON：`{"sql":"..."}`，必须带 Content-Type）：

```bash
curl -s -X POST "$SERVER/v1/sql" \
     -H "Authorization: Bearer $WS_KEY" \
     -H "Content-Type: application/json" \
     -d '{"sql":"SELECT path, snippet FROM veda_search WHERE query = '\''如何启动'\'' LIMIT 5"}' | jq
```

或者直接发搜索 HTTP 请求：

```bash
curl -s -X POST "$SERVER/v1/search" \
     -H "Authorization: Bearer $WS_KEY" \
     -H "Content-Type: application/json" \
     -d '{"query":"如何启动","mode":"semantic","limit":5}' | jq
```

> `mode` 取值：`fulltext` / `semantic` / `hybrid`（默认 `hybrid`）。

---

## 6. 看一下指标

Grafana 在 `http://localhost:3001`（默认 admin/admin）。Prometheus data source 已配好，但**没有预置面板** —— 试用时建议至少手动加这几个：

| 指标 | 类型 | 用途 | 建议 PromQL |
|---|---|---|---|
| `veda_outbox_process_seconds` | histogram | worker 处理延迟 | `histogram_quantile(0.99, sum by (le) (rate(veda_outbox_process_seconds_bucket[5m])))` |
| `veda_embed_latency_seconds{outcome="ok"}` | histogram | embedding API 耗时 | `histogram_quantile(0.95, sum by (le) (rate(veda_embed_latency_seconds_bucket{outcome="ok"}[5m])))` |
| `veda_embed_total{outcome=...}` | counter | embed 成功 / 失败计数 | `sum by (outcome) (rate(veda_embed_total[5m]))` |
| `veda_drift_total{kind=...}` | gauge | reconciler 漂移修复计数（应该接近 0） | `veda_drift_total` |
| `veda_http_requests_total{status=...}` | counter | HTTP 错误率 | `sum by (status) (rate(veda_http_requests_total[5m]))` |

> ℹ️ **`/v1/metrics` 返回 404 不是 401**：endpoint 用 404 隐藏自己的存在以避免被遍历探测。如果你确认 token 对了但仍 404，可能是 token 文件多了换行或引号；回 §3.4 检查。

---

## 7. 已知限制（Alpha）

### 性能与规模

- 单文件硬上限 50MB（`MAX_FILE_BYTES`），超过会拒写
- 通过 FUSE 写入 >10MB 文件可能较慢（流式还在调）
- 单副本部署，多副本协调能力等下一阶段（计划用 ZooKeeper 做集群）

### 权限与安全

- 仅 workspace 级权限，所有成员可读可写所有文件
- 无登录失败次数限制 / 无 IP 频控 / 无 token 主动撤销
- JWT / API key 24h 自动过期；生产部署强烈建议放在内网

### FUSE 客户端

- 改 chmod / chown 静默忽略
- 文件稀疏写（如 seek 到 1GB 写 1 字节）会把空洞按零字节加载到内存
- SSE 长连接每 120 秒强制断开重连一次（`SSE_REQUEST_TIMEOUT` hard-coded const）—— 通常无感，只是日志会有 reconnect 行
- 客户端离线超过 14 天再连，会收到 server 410 → 自动清 cache + 跳到 `current_max_id`，依赖 FUSE 内核 `readdir` / `getattr` 懒回源（不会主动 list_dir）

### CLI

- `veda cp` 单文件全量读入内存，不要用于 >10MB 文件
- 目录递归上传 (`veda cp ./dir /remote -r`) 是 sequential 的，跳过 symlink；任一单文件仍受 50MB 上限

### 搜索

- Hybrid = dense ANN + BM25 sparse, RRF 融合（Milvus 2.5 hybrid_search）
- Fulltext = BM25 over 同一 sparse 字段；中文 BM25 用 jieba 分词
- 搜索结果引用 `chunk_index` 不稳定，文件重写后历史引用可能错位
- `score` 数值跨模式不可比；客户端读 `score_type`（`rrf` / `bm25` / `cosine`）后再做大小判断

### 文档类型

- 仅支持 text/plain。PDF / 图片 OCR 未实现，二进制文件可写但不会被向量化

### 部署

- Worker 内嵌于 server 进程（`tokio::spawn(Worker::run)`），多副本部署不支持
- 推荐 docker-compose 或单机 systemd
- compose 路径每次 `docker compose up` 都会重跑一次 migrate（idempotent，但多花几秒）；不想重跑就用 systemd 路径或单独 `docker compose up -d veda-server`

### 数据正确性

- 同一文件写入与删除若在 worker 同批次内并发处理（概率低），可能短期向 Milvus 留下孤儿向量
- Reconciler 默认每 6 小时（`reconciler.interval_secs = 21600`）跑一次，会清理孤儿
- `veda_fs_events` 默认保留 14 天（`retention.events_retention_days = 14`）；客户端如果离线超过 14 天，重连会收到 410 并 cursor 跳到 `current_max_id`

---

## 8. 反馈与停服

### 反馈

GitHub issue 是首选：[github.com/jooekong/veda/issues](https://github.com/jooekong/veda/issues)。

最关心两类 bug：

1. **数据不一致** —— 写进去的内容读出来不一样、`ls` 看不到刚写的文件、搜索结果引用了已删除的文件
2. **僵死** —— FUSE 阻塞 / server CPU 100% / worker 日志反复 retry 同一个 outbox 任务

issue 里附以下信息能加快定位：

- `docker compose ps` 输出
- `docker compose logs --tail=200 veda-server`
- 相关时间窗的 Prometheus 指标截图（`veda_outbox_*`, `veda_drift_*`）

### 完全重置（"我搞砸了"）

```bash
cd veda/deploy
docker compose down -v        # 删 mysql / milvus / etcd / minio 所有数据
rm -rf secrets/               # 删 metrics token
rm -f .env config.docker.toml # 删本地配置
# 然后从 §3.2 重来
```

### 停服（保留数据）

```bash
docker compose down            # 容器停掉，volumes 保留，重启不丢数据
```

systemd 路径：

```bash
sudo systemctl stop veda-server
sudo systemctl disable veda-server
```

---

## 9. 拓展阅读

- [`docs/plans/alpha-plan-2026-04-29.md`](plans/alpha-plan-2026-04-29.md) —— 6 周开发计划，看完整路线图
- [`docs/reviews/module-review-2026-04-29.md`](reviews/module-review-2026-04-29.md) —— 模块级 review，列了所有已知设计权衡
- [`docs/design/`](design/) —— 各模块设计文档

试用愉快！踩到坑随时 ping 我。
