# Embedding 吞吐优化方案

> 状态:**阶段 1 已实现(2026-07-29 最终形态:两级优先闸)**——`TwoLevelGate`(High=交互 search/ask/同步 vectors 写,Low=worker 异步索引,worker 经 `EmbeddingProvider::background()` 低优先视图接入):空闲时后台可占满全部 permit,交互调用到达即获得**下一个释放的号**(物理下限=等一次在飞调用 ~200-500ms);同级 FIFO;取消的等待者在派号时被跳过,号不丢。全部调用直发:按 batch_size 切块、`buffered(4)` 限单调用扇出(同级内公平)、**429 backoff 期间还号**、acquire 无超时(各调用方自己的 deadline 兜底)。config `[embedding] max_concurrency=8`;指标 `veda_embed_permit_wait_seconds{priority}`/`inflight`/`429_total`/`batch_texts`。验证:5 个闸行为单测(插队/空闲占满/取消不吞号/同级保序/并发上限)+ 真实 airouter 集成 7/7;压测复跑待部署后对比 §背景 36 QPS 基线。
> 演进记录(07-29 当天三步):①先实现「闸+跨请求攒批」;②Codex(xhigh)+Claude 交叉 review 修四处(攒批单飞串行/分流漏斗/permit 超时烧 retry 预算/gauge 取消泄漏);③Joe 简化审查拍板**整体撤回攒批器**——其收益前提(search QPS 数百)与现实(个位数,429 起爆点 600 req/min≈10 QPS)差两个数量级,而复杂度当天就贡献了 review 4 个 MAJOR 中的 3 个;30s permit 超时随之一并删除。攒批完整实现(含全员放弃中止等修复)在 git 历史,**重启触发条件:search 侧 429 率持续可见或实测 QPS 逼近 10**。「优先级闸」形态优于备选「后台限额」(限额会浪费空闲时段 3/4 吞吐)——Joe 直接点破本质后采纳。已知边界(第二轮交叉 review 确认):①High 持续满载时 Low 理论上无限等待——现实 QPS 到不了,且后果不是死信而是 worker 任务**静默卡 processing**(renewer 持续续租,租约永不过期),观测抓手=`veda_embed_permit_wait_seconds{priority="low"}` 高分位 + `veda_outbox_depth{status="processing"}`,真出现再加 aging;②单条 search 直发使突发场景 RPM 高于攒批形态(均匀低 QPS 下无差),已含在重启触发条件内;③permit_wait 只记录成功获取的等待(被取消的排队不产生样本),放弃可观测性列 backlog。
> **阶段 2(upsert sync/async)未实现**,等阶段 1 生产数据再启。
> 压测实锤见 [`docs/loadtest-2026-06-05.md`](../loadtest-2026-06-05.md)。

## 背景

2026-06-05 压测把 embedding 定为 search/upsert 的吞吐天花板:

| 场景 | 吞吐 | p99 | 错误率 | 主导层 |
|---|---|---|---|---|
| hybrid search(cache 命中,隔离 Milvus) | 153 QPS | 916ms | 0% | Milvus |
| hybrid search(真实 embedding) | **36 QPS** | **3.94s** | **25% (429)** | **embedding 1235ms** |
| upsert(真实 embedding,20/req) | 17 QPS | 7.4s | **29.6%** | embedding |

## 限流本质(关键前提,2026-06-08 确认)

- 限流在**云商平台**(text-embedding-v4 上游),不是 airouter;airouter 只是网关。
- 限流维度是 **QPM / 请求数**(Joe 判断:"做 batch 能增吞吐" → 卡在请求数维度)。
- **配额是硬天花板**,提不了 / 不易提。
- 单请求最多 **10 条**(`config/server.toml` `batch_size=10`,Bailian 上限)。

→ **优化第一性原理变成:在固定的「请求数/分钟」预算里塞进最多的数据 + 不让突发打爆。**

> ⚠️ 待确认:若限流实际在 **token/min(TPM)** 维度,则攒批不省 token、无效。下手前用 1VU 基线 + 观察 429 触发确认是 RPM 维度。

## 问题结构:1 个根因 + 2 个独立表现

**根因**:`crates/veda-pipeline/src/embedding.rs` 有 retry/backoff 但**无客户端并发闸**。高并发下每请求各自打上游 → 429 → 各自 backoff 重试 → 放大压力(雪崩),重试期间还占着请求槽。

装配上是**单一 `EmbeddingProvider` 实例**(`main.rs:74`),被 vectors 数据面、fs search、collection、sql 所有路径 `clone` 共享 → **并发闸加在 provider 里就是天然全局闸**,自动覆盖所有路径。

| 表现 | 真实瓶颈 | batch_size=10 相关 | 代码定位 |
|---|---|---|---|
| **search**(query 单条) | 上游单次往返 ~1.2s + 无闸雪崩 | ❌(只 1 条) | `vectors.rs:322` |
| **upsert 大批量** | batch_size 切分 + 批次**串行** await | ✅ | `embedding.rs:268-270` 串行 `for ... await` |

## 设计:读写分治(两条路径平衡点不同)

### 读(search):延迟敏感,不能异步 → 攒批 + 双触发窗口

> ⚠️ **本节为历史设计存档**——攒批器已于 07-29 实现后整体撤回(见头部状态框的演进记录与重启触发条件),当前实现为两级优先闸+全部直发。以下内容触发重启时参考。

跨请求 micro-batching:多个并发 query 的单条 text 攒成一批(≤10)发一次。

- **双触发,先到先发**:凑满 10 条 **OR** 窗口超时(如 5–10ms)。
- **自适应平衡**:高负载瞬间凑满 → 延迟几乎不加、吞吐拉满;低负载窗口超时即发 → 延迟 +Nms、但低负载不缺吞吐。
- **划算**:single query 已 1.2s,+5–10ms 窗口 <1% 不可感知,换 ~10x 吞吐。窗口大小做 config(偏延迟设小 / 偏吞吐设大)。

实现形状(batching executor):每个 `embed()` 调用把 `(text, oneshot_sender)` 丢进 mpsc;一个后台 task 收集,凑满 / 超时触发一次 batch 请求,按 `index` 把结果 oneshot 回各调用者。全局闸(Semaphore 限并发 batch 数)内化在这个组件里。

### 写(upsert):延迟不敏感(可接受最终一致)→ 异步两步解耦

把"写 text"和"算 embedding"拆开,正是 veda **fs 路径已有的 outbox+worker 模型**(写文件 → outbox → worker 异步 embed+写 Milvus)。目前**只有 db 数据面 upsert 还走同步 embedding**(`vectors.rs:117`)。

```
upsert → 写 text+meta 入 pending + 入队 → 立即返回 202
                                          ↓ (后台)
                           worker 批量 claim → 攒批 embed → 写 Milvus
```

- **价值**:写响应 1.2s+ → 几十 ms;embedding 后台受控攒批消化,彻底削峰,用户侧再无 429;响应时间与吞吐**解耦**。
- **额外好处**:异步后攒批变简单——worker 本就批量 claim,outbox 队列天然是攒批缓冲区,直接 `chunks(10)` 发,不需要同步窗口/oneshot。
- **代价**:upsert 变**最终一致**(写完不能立即 search);需 backlog 可观测(pending 计数 / 消费延迟)。

**Milvus 能力边界（2026-06-11 查证 2.6 文档，钉死本设计）：**

- **向量字段不可 nullable**（"Vector, JSON, and Array fields do not support nullability"，仅标量可空）
  → "text 先进 Milvus、embedding 后补"**不可行**，无向量的行写不进 collection。占位零向量绕路
  也否决：窗口期零向量行会进 semantic/hybrid 结果（垃圾命中）+ 每行写两次，写放大更差。
  **async 的"先写 text"只能落自己的 MySQL pending 区，Milvus 只接收完整行**——即上图 outbox
  形态，Milvus 零改动。
- **可见性语义**：async 模式写完立刻 search 什么都查不到——fulltext 的 BM25 sparse 虽由 Milvus
  从 text 服务端生成（天然 text-first），但整行没进去就都不可见。202 + 最终一致须写进 API 文档。
- **2.6 新能力 `partial_update`（merge 模式 upsert，只改指定字段）**：与 async 无关，但解锁
  "只改 meta 不动 text"的零 embedding 更新——现状整行 upsert 改个 meta 也要重新给向量（不命中
  cache 就重 embed 烧配额）。列为阶段 2 之外的后续选项；前置：确认公司集群 2.6 小版本 +
  REST `partialUpdate` 参数支持。

## 决策(2026-06-08)

| 项 | 决策 |
|---|---|
| **时间线** | **先上线,后优化**。本方案先固化文档,不阻塞上线。 |
| **阶段1(不改契约)** | **全局闸 + 读路径攒批器一起上**(同一个 batching+rate-limit executor 组件)。防雪崩 + search ~10x。 |
| **阶段2(改契约)** | upsert **加参数让调用方选 sync/async**:`sync` 默认强一致(写完即可搜);`async` 走队列(最终一致,202)。复用 outbox+worker。 |

> 阶段2 是双模式参数(非纯异步),保留强一致默认,async 由调用方 opt-in。

## 阶段路线

| 阶段 | 内容 | 改契约 | 复杂度 | 收益 |
|---|---|---|---|---|
| 1 | 全局 Semaphore 闸 + 读路径攒批 executor;闸/窗口/batch 全 config | ❌ | 中 | 防雪崩 + search ~10x,确定 |
| 2 | upsert `write_path=sync\|async`,async 走 outbox+worker 攒批 | ✅ | 中–大 | 写响应几十 ms + 削峰 |
| — | 容量侧:云商提配额 / 专用通道(不在 veda 代码内) | — | — | 抬高硬天花板 |

## 实现要点(下手时直接用)

- **Semaphore 放 `EmbeddingProvider`**:单实例共享 → 天然全局,自动覆盖 fs+db 所有路径。在 `embed_single_batch` 发 HTTP 前 `acquire`。
- **429 快速降级**:permit 久等 / 429 超 `Retry-After` 阈值 → 直接 503 让调用方退避,不无脑重试放大(现状 `embedding.rs:224` 无脑 backoff 重试)。
- **新增 config**(`[embedding]`):`max_concurrency`(闸,默认保守值如 8)、`batch_window_ms`(攒批窗口,如 5–10)、沿用 `batch_size`(=10)。
- **可观测**:in-flight gauge、permit 等待时间、429 计数、降级 503 计数、攒批实际批大小分布。
- **攒批器服务范围**:读路径(search/query embedding)优先;upsert 同步路径也可受益。大批调用(本就 ≥batch_size)可 bypass 攒批直接发。

## 待确认 / 未知（1、2 已由 2026-06-11 生产压测回答，见 `docs/loadtest-prod-2026-06-11.md`）

1. ✅ **限流维度 = RPM（请求数）**：直打 airouter 实测 batch=10（6,000 条/min）零 429、
   batch=1（600 条/min）才见 429 → **攒批有效，×10 载荷无配额惩罚**。起爆点 ≥600 req/min，
   但配额全公司共享、随邻居突发波动（同参数一次 5×429、复测干净），闸按保守预算设而非实测
   天花板；429 响应**无 Retry-After 头**，降级退避须自带。
2. ✅ **1VU 空载基线：单条 p50 ~200ms（p99 302ms）**，满批 10 条 446ms（44.6ms/条，4.4× 摊销）。
   旧报告的 1.2s 是 50VU 排队放大。攒批窗口 +5–10ms 相对 200ms <5%，代价确认可忽略。
3. **阶段2 默认值**——建议 `write_path` 默认 `sync`(强一致),`async` opt-in;最终值待定。
