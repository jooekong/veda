# db 向量数据面压测

对 `/v1/vectors/{upsert,search,query,delete}` 做阶梯加压找性能瓶颈。本地 mac 连内网
Milvus/MySQL + 真实 airouter embedding，靠服务端三层延迟直方图做分层归因。

## 思路

连真实 embedding 拿到的是真实端到端延迟，再用代码里已埋的三层嵌套直方图做减法，
把延迟拆到层：

```
embedding+框架 = veda_vector_request_seconds − veda_vector_store_op_seconds
store(非Milvus) = veda_vector_store_op_seconds − veda_milvus_request_seconds
Milvus         = veda_milvus_request_seconds
```

阶梯加压（ramping-vus）升并发，**拐点 = QPS 不再涨而 p99 拐头**，那一层就是瓶颈。

## 前置

```bash
# 1. release 编译（debug 的 CPU 数字失真，必须 release）
cargo build --release -p veda-server

# 2. 起 server，用环境变量注入 metrics scrape token（不动 server.toml 里的内网密钥）
VEDA_METRICS_TOKEN=loadtest-scrape-token ./target/release/veda-server config/server.toml

# 3. 装 k6
brew install k6
```

## 跑

**一键全跑**（灌库 + 8 轮 + 归因采样，日志落 `/tmp/loadtest/`）：

```bash
cd scripts/loadtest
node provision.mjs
METRICS_TOKEN=loadtest-scrape-token ./run_all.sh
```

或**分步手动**：

```bash
cd scripts/loadtest

# 1. 建隔离的 db workspace + 写权限 wk_，落 .env.loadtest
node provision.mjs

# 2. 灌 2 万种子（1000 句模板池 + embedding cache 命中，省配额）
node seed.mjs                      # 可调 TOTAL=50000 node seed.mjs

# 3. 起归因采样（单开一个终端，压测期间一直跑）
METRICS_TOKEN=loadtest-scrape-token node sample_metrics.mjs --op search

# 4. 分端点阶梯加压（从干净到重，每轮单独跑）
source .env.loadtest
k6 run -e OP=query  -e BASE=$BASE -e WK=$WK k6_vectors.js   # 无 embedding，摸纯底
k6 run -e OP=search -e MODE=hybrid   -e BASE=$BASE -e WK=$WK k6_vectors.js
k6 run -e OP=search -e MODE=semantic -e BASE=$BASE -e WK=$WK k6_vectors.js
k6 run -e OP=search -e MODE=fulltext -e BASE=$BASE -e WK=$WK k6_vectors.js   # 不 embed
k6 run -e OP=upsert -e BASE=$BASE -e WK=$WK k6_vectors.js
k6 run -e OP=delete -e BASE=$BASE -e WK=$WK k6_vectors.js   # ⚠ 删种子，跑完重 seed
```

## 怎么读结果

- **k6 实时面板**：盯 `RPS` 和 `p(95)/p(99)`。RPS 升到某并发后压平、同时 p99 开始陡升 = 拐点。
- **sample_metrics 表**：看 `embed+fw / store-mv / milvus` 哪列吃掉端到端的大头，`pool_use`
  逼近 50（max_connections）说明 MySQL 连接池饱和。
- k6 summary 的 `http_req_failed`、`http_req_duration p(99)` 是该轮总结；阈值只是标红参考，不中断。

## 生产压测（负载机打 .85）

绝对容量数字只能从生产环境拿。负载机用同机房内网 box（如 .161，RTT <1ms），
**不要**让 k6 和 server 同机争核（.85 只有 4C，上轮同机跑读 QPS 被压低）。

```bash
# mac 上：provision + seed（控制面低频调用，跨网无所谓）
node provision.mjs --base http://10.79.55.85:3000
WRITE_MODE=insert node seed.mjs        # 首灌空库才用 insert；重灌走默认 upsert
scp .env.loadtest root@<负载机>:/data/rust/veda/scripts/loadtest/

# mac 上：归因采样（METRICS_TOKEN 从 .85 config 拿）
METRICS_TOKEN=... node sample_metrics.mjs --op search

# 负载机上：k6 轮次（示例）
source .env.loadtest
k6 run -e OP=query -e MAX_VUS=400 -e BASE=$BASE -e WK=$WK k6_vectors.js          # 读容量
k6 run -e OP=upsert -e UNIQUE_TEXT=0 -e WRITE_MODE=insert -e BASE=$BASE -e WK=$WK k6_vectors.js  # 写快路径
k6 run -e OP=search -e MODE=semantic -e BASELINE=100 -e BASE=$BASE -e WK=$WK k6_vectors.js       # 1VU 基线
k6 run -e OP=mixed -e VUS=50 -e DURATION=30m -e UPSERT_BATCH=10 -e BASE=$BASE -e WK=$WK k6_vectors.js  # soak
```

限流维度判别（RPM vs TPM，embedding-throughput-plan 的前置确认）直接打 airouter，
凭证从 box 的 `/data/veda/config/config.toml` 注入环境变量，不进命令行：

```bash
AIROUTER_API_URL=... AIROUTER_KEY=... node embedding_ratelimit_probe.mjs --batch 1
AIROUTER_API_URL=... AIROUTER_KEY=... node embedding_ratelimit_probe.mjs --batch 10
# 两次 429 起爆点同一 req/min ⇒ RPM；batch=10 提前 ~10× ⇒ TPM/条数维度
```

⚠️ 生产注意：real-embedding 轮和 probe 烧的是**全公司共享 airouter 配额**（会 429
别的调用方），挑低峰窗口；写容量轮前知会 Milvus DBA。跑完两步清理：
`DELETE /v1/workspaces/{id}`（vk_ 鉴权，软删+吊销 key）+ `cleanup_test_data.py`
指向生产 Milvus drop collection。

## 对照实验（分离 embedding 变量）

`UNIQUE_TEXT` 开关决定 query/text 是否唯一：

| | `UNIQUE_TEXT=1`（默认） | `UNIQUE_TEXT=0` |
|---|---|---|
| 每次请求 | query/text 唯一 → **真实 embedding** | 固定池 → embedding cache 命中 |
| 暴露什么 | embedding 真实瓶颈（batch_size=10、限流） | 隔离出纯 Milvus + 框架吞吐 |

同一端点跑两遍对比，`embed+fw` 列的差值就是 embedding 的真实贡献。

```bash
k6 run -e OP=upsert -e UNIQUE_TEXT=1 -e BASE=$BASE -e WK=$WK k6_vectors.js   # 含真实 embedding
k6 run -e OP=upsert -e UNIQUE_TEXT=0 -e BASE=$BASE -e WK=$WK k6_vectors.js   # cache 命中
```

## 预判的瓶颈候选（重点验证）

1. **embedding `batch_size=10`**：upsert/search 大批量会被切成很多次串行外部调用。
2. **每请求 `resolve_dataset` 查一次 MySQL + 池上限 50**：高 QPS 下 MySQL 往返 / 连接池 acquire timeout。
3. **Milvus hybrid（默认 mode）**：dense + BM25 sparse 双路 RRF，比 semantic 重。

## 其他专项脚本

本 README 只覆盖核心压测链路；目录下另有 `write_mode_e2e.mjs`（insert/upsert 语义
e2e）、`embedding_ratelimit_probe.mjs`（限流维度判别，直打 airouter）、
`cleanup_test_data.py`（drop 指定 ws 的 Milvus collection，--mv/--token/--db 必填）、
`milvus_*.py`（Milvus 裸压/分诊系列），用法见各脚本顶部注释。

## 注意

- 本地 mac 非生产配置，**绝对数值不代表线上**，用途是定位瓶颈和相对对比。
- `delete` 会消耗种子 id；跑完重新 `node seed.mjs`，或用 `SEED_PREFIX` 指向独立 id 段。
- `UNIQUE_TEXT=1` 走真实 embedding，会烧 airouter 配额；规模大时留意限流。
- 参数（`MAX_VUS / UPSERT_BATCH / SEED_TOTAL` 等）见各脚本顶部注释。
