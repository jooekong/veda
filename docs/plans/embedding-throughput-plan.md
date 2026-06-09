# Embedding 吞吐优化方案

> 状态:**已设计、未实现**。决策见下「决策(2026-06-08)」。优先级:**先上线,后优化**。
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

## 待确认 / 未知

1. **云商配额数字 + 限流维度**(RPM vs TPM)——定闸/窗口参数的前提;TPM 维度则攒批无效。
2. **1VU 空载单条延迟基线**——那 1.2s 含 50VU 下排队,空载纯往返多少?决定延迟本身有多糟。补一个 1VU 轮测。
3. **阶段2 默认值**——建议 `write_path` 默认 `sync`(强一致),`async` opt-in;最终值待定。
