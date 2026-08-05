# db 向量数据面生产压测报告（2026-06-11）

- **被压**：`.85` 生产节点（华为云 4C/8G，`7e5d5bc` / 0.1.13，本轮升级后），连生产 MySQL + 生产 Milvus（db `veda`）+ 真实 airouter embedding
- **负载机**：`.161`（同 DC，HTTP RTT 0.8ms）跑 k6；mac 经 SSH 隧道跑 provision/seed/归因采样
- **方法**：沿用 `scripts/loadtest/` 三层直方图减法归因；本轮新增 `WRITE_MODE` / `BASELINE` / `OP=mixed` / 限流探针（见 README）
- **底库**：2 万种子（insert 快路径灌库 29.5s，678 rec/s 经隧道）；R4 写轮后涨至 ~200 万行，soak 在大底库上跑
- **对比基线**：06-05 mac 轮（仅相对参考）、06-09 .85 同机自压轮（k6 与 server 争核，数值保守）

> 本轮目的：回答 06-09 轮遗留的四个问题——真实读容量（外部负载机）、embedding 限流维度
> （RPM vs TPM）、insert 快路径并发收益、混合负载长稳。结论：**四个全部有答案，读写容量
> 远超预期，B-A1 池饱和担忧被证伪，embedding 攒批方案收益盖章。**

---

## TL;DR

| 问题 | 答案 |
|---|---|
| 真实读容量 | query **5,650 QPS** / fulltext **7,150** / hybrid(cache) **2,590**，p99 全部 <90ms，**0 错误** |
| MySQL 池（B-A1） | 5.6k QPS 下 pool 峰值 **10/50**，框架+MySQL 层 **0.4ms** —— **非 blocker，证伪** |
| embedding 限流维度 | **请求数（RPM），非 TPM**：batch=10（6,000 条/min）零 429，batch=1（600 条/min）才见 429 |
| 限流起爆点 | **≥600 req/min**（共享配额有邻居噪声：同参数一次 5×429、复测干净）；429 **无 Retry-After 头** |
| embedding 空载基线 | 单条 **p50 196–206ms**（mac 轮的 1.2s = 50VU 排队水分）；满批 10 条 **p50 446ms**（摊 44.6ms/条，**4.4× 摊销**） |
| insert 快路径并发收益 | **156 QPS（~7,800 条/s）vs 默认 upsert 30 QPS（1,500 条/s）= 5.2×**，p99 462ms vs 1s |
| 30min 混合 soak | 37.4 万请求 **0 错误**，RSS 平稳 580MB（无泄漏迹象），p99 606ms @200 万行底库 |

---

## 轮次明细

### R1 · 1VU 空载基线（真实 embedding）

| 轮 | 参数 | p50 | p99 | 错误 |
|---|---|---|---|---|
| R1a semantic search | 1VU×100，唯一 query | 202ms | 302ms | 0 |
| R1b upsert | 1VU×30，10 条唯一文本/请求（=1 次满批 embedding） | ~1.0s | 1.07s | 0 |

- 06-05 报告的"单条 embedding ≈1.2s"修正为 **空载 ~200ms**，1.2s 是 50VU 并发下的排队放大。
- 满批 10 条 993ms ≈ 99ms/条 vs 单条 202ms：**攒批摊销约一半**，读路径攒批的延迟代价完全可接受。

### R2 · 限流维度判别（直打 airouter，`embedding_ratelimit_probe.mjs`）

| 形态 | 梯子 | 结果 |
|---|---|---|
| batch=1 | 300→600 rpm | 300 干净；600 出 5×429（复测同参数 **0×429** → 判定为共享配额邻居突发） |
| batch=10 | 60→600 rpm | **全干净**，含 600 rpm = **6,000 条文本/min** |

- **维度=请求数（RPM）**：同请求速率下 10× 文本量不增加 429 概率 → 攒批把有效吞吐 ×10，方案第一性原理成立。
- 起爆点 ≥600 req/min，但配额全公司共享、随邻居波动，**不要按"实测天花板"设闸**，按保守预算设。
- 429 响应**无 Retry-After**（body `rate_limit_exceeded`）→ 降级逻辑需自带退避，不能依赖响应头。

### R3 · 读容量（.161 外部加压，阶梯 ramping）

| 端点 | MAX_VUS | 峰值 QPS | avg | p99 | 错误 | 归因（峰值段） |
|---|---:|---:|---:|---:|---:|---|
| query | 400 | **5,650** | 28.7ms | 84ms | 0 / 110.9 万 | Milvus 24ms；框架+MySQL **0.4ms**；pool ≤8/50 |
| search fulltext | 300 | **7,150** | 18.0ms | 60ms | 0 / 132.8 万 | Milvus 8ms；pool ≤10/50 |
| search hybrid（cache 命中） | 200 | **2,590** | 32.6ms | 88ms | 0 / 48.8 万 | Milvus 16ms（双路 RRF） |

- 对比 06-09 同机自压：query 4,642→5,650、fulltext 3,284→7,150、hybrid 1,291→2,590 ——
  **争核水分 ~2×**，本轮才是可对外承诺的数字（且 .85 仅 4C，纵向还有空间）。
- **B-A1 证伪**：每请求 2 跳 MySQL（鉴权+resolve_dataset）在 5.6k QPS 下只占 0.4ms、池 10/50。
  moka 缓存/池扩容均无必要，pre-GA 条件项建议关闭。

### R4 · 写容量对照（60VU × 50 条/请求，cache 命中隔离 embedding）

| 写模式 | 峰值 QPS | 条/s | avg | p99 | 错误 |
|---|---:|---:|---:|---:|---:|
| 默认 upsert（dedup+delete+insert） | 30 | ~1,500 | 757ms | 1.0s | 0 / 6,268 |
| **write_mode=insert** | **156** | **~7,800** | 144ms | 462ms | 0 / 32,798 |

- 并发下 insert 快路径 **5.2×**（顺序 e2e 测的 3× 在并发下进一步放大——dedup 查重在 Milvus 侧排队更贵）。
- 默认 upsert 30 QPS 与 06-09 完全一致（写瓶颈在 Milvus 侧 dedup 链路，与负载机无关，符合预期）。
- 灌库口径（seed.mjs insert）：678 rec/s 经 mac 隧道为下限；R4b 实测同 DC 可到 7,800 rec/s。

### R5 · 30min 混合 soak（50VU 恒定，query45/fulltext25/hybrid20/upsert10，全 cache 命中）

- **373,987 请求 / 平均 207.7 QPS / 30 分钟 0 错误**，checks 100%（747,974/747,974）。
- avg 240ms / med 203ms / p99 606ms / max 2.15s。注意两点口径：底库是 R4 灌出的
  **~200 万行**（比 R3 时的 2 万行重两个数量级），且混入 10% 默认 upsert 慢路径——
  延迟不能与 R3 直比，soak 的结论是**稳定性**而非容量。
- **内存零泄漏迹象**：RSS 564MB（8min）→ 580MB（16min）→ 580MB（结束），距 MemoryMax=4G
  充裕；全程 journal 0 ERROR，平稳无衰减。

---

## 结论与后续

1. **读侧上线无忧**：4C 单节点 2.6k–7.2k QPS、p99 <90ms、百万级请求零错误。
2. **B-A1 关闭**：MySQL 控制面热路径不是数据面瓶颈（0.4ms、池 1/5 用量）。`6e6d4bf`
   的单查询鉴权之后，无需再加缓存层。
3. **embedding-throughput-plan 三个"待确认"全部回答**（详见该文档回填）：
   RPM 维度成立 → **阶段 1（全局闸 + 读路径攒批）可动手**，预期把真实 embedding search
   的有效吞吐 ×10 并消灭 429 雪崩；阶段 2（upsert sync/async）收益由 R4 数字背书。
   闸参数建议：max_concurrency 从保守值（如 8）起步，429 即退避（无 Retry-After 可依赖）。
4. **写路径指引**：批量导入/首灌一律 `write_mode=insert`（7,800 条/s）；常规更新走默认
   upsert（1,500 条/s 已够，且语义安全）。
5. 压测哨点：`k6_vectors.js` 的 tag 已加跑次盐值（SALT）——此前跨 run 重放同名文本会
   静默命中 embedding cache、insert 模式撞 id，老数据若复测须留意。

## 复现与清理

```bash
# 升级三台至 7e5d5bc 后：
node provision.mjs --base http://<tunnel>:13000      # mac 经 SSH 隧道
WRITE_MODE=insert node seed.mjs                      # 首灌空库
# k6 在 .161：见 scripts/loadtest/README.md「生产压测」一节
# 清理：DELETE /v1/workspaces/{ws}（vk_）+ cleanup_test_data.py --mv <prod> --db veda <ws>
```

原始日志：`.161:/tmp/loadtest-prod/*.log`、`.85:/tmp/loadtest-prod/probe_*.log`、
mac `/tmp/r3*_*.sample.log`。本轮 ws `c50ba149-306c-438b-9cc4-b16fca659d42`（已清理）。
