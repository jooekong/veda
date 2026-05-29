# Outbox 去重激进重构

> 状态：待开工（仅记录，未改代码）
> 来源：2026-05-29 deep review 的去重方案评审，Joe 选定「激进重构」方向

## 动机

当前「去重」散落在 3 层、6 个组件里，复杂度与实际收益不匹配：

| 组件 | 位置 | 问题 |
|------|------|------|
| `try_insert_outbox_for_file` | store trait + `mysql.rs:1714` | 与 `has_pending_event` 是两份几乎一字不差的 SQL |
| `has_pending_event` | store trait + `mysql.rs:1978` | 同上；写热路径上的 JSON_EXTRACT 全扫描，无索引 |
| `enqueue_dedup` | `server/outbox.rs:23` | 包装层 |
| worker `enqueue_summary_sync` + burst debounce | `worker.rs:336-395` | 注释自认 "Currently unused"，死代码 |
| worker `enqueue_dir_summary_sync` | `worker.rs:471` | debounce 仅对 DirSummarySync 半生效 |
| reconciler `enqueue_*` 三个包装 | `reconciler.rs:565-625` | 各自又包一层 enqueue_dedup |

核心判断：
- **ChunkSync 的去重价值已被 watermark 架空**——`worker.rs:265` 的 `last_embedded_content_hash == checksum` 短路意味着重复 ChunkSync 第二次就是 no-op，去重和 watermark 功能重叠。
- **去重真正有价值的只有 SummarySync / DirSummarySync**（无短路，省 LLM 钱）。
- burst debounce 对 SummarySync 已失效（service 层直插，`available_at=now`），精确去重的实际命中窗口只有 `poll_interval`（1s）。

## 目标

删除 enqueue 期去重三件套，enqueue 全部改为裸 insert；去重降级为 **worker claim 后的内存 coalesce**（best-effort，batch 内合并）。

## 改动清单

### 删除
1. `try_insert_outbox_for_file`：trait `store.rs:350` + impl `mysql.rs:1714`
2. `has_pending_event`：trait `store.rs:428` + impl `mysql.rs:1978`（但见「注意点 1」，reconciler 仍需替代查询）
3. `enqueue_dedup`：`server/outbox.rs`（确认无其他引用后整文件可删）
4. worker 死代码：`enqueue_summary_sync` + burst 逻辑 `worker.rs:336-395`、`SUMMARY_DEBOUNCE_SECS`、`in_burst` 判断

### 改为裸 `insert_outbox` / `enqueue`
- 写主路径 `fs.rs`：`371, 379, 1213, 1215, 1459, 1461, 1493, 1495` 全部 `try_insert_outbox_for_file` → `insert_outbox`
- reconciler `565-625`：`enqueue_chunk_sync_force` / `enqueue_summary_sync` / `enqueue_dir_summary_sync` 去掉 dedup 包装，裸 enqueue
- worker `471`：`enqueue_dir_summary_sync` 裸 enqueue

### 新增：worker claim 后 coalesce
在 `poll_once` 拿到一批 claim 结果后：
- 按 `(workspace_id, event_type, entity_id)` 分组
  - `entity_id`：ChunkSync/SummarySync = `payload.file_id`；DirSummarySync = `payload.dentry_id`；ChunkDelete 不 coalesce
- 每组只处理一条（保留最大 `id` = 最新），其余直接 `task_queue.complete()`
- 安全性保证：同 `(ws, type, entity)` 的多条任务都是「读最新状态重新同步」，语义等价，丢弃旧条目安全

## 关键注意点（不能漏）

1. **reconciler orphan 删除前的 re-confirm 依赖 `has_pending_event`**（`reconciler.rs:335`，"无在途 ChunkSync 才删 orphan"，是误删安全网）。删掉 `has_pending_event` 后必须提供替代：一个明确命名的轻量查询（如 `count_inflight_for_file`，查该 file_id 有无 pending/processing 行）。**不能简单删掉这层防护。**
2. **coalesce 只在同一 claim batch 内**，跨 batch 重复仍会重复处理：
   - ChunkSync：watermark 短路吸收，不会真重复 embed。✅
   - SummarySync/DirSummarySync：无短路，跨 batch 重复会多跑 LLM。**已接受的退化**（选激进方案时知情）。
3. **慢速持续编辑**（编辑分散在多个 1s poll 周期）coalesce 不掉，summary 省不了 LLM。已接受。
4. **outbox 行数会变多**（不再 enqueue 去重），依赖 `prune_outbox_older_than` 清理；确认 retention 间隔/cutoff 够用。
5. **idx_dedup 索引不必加**——激进方案下没有 enqueue 期去重查询。但「注意点 1」的替代查询仍需走索引，评估是否要 `(workspace_id, event_type, status)`。
6. **「只比 pending」竞态注释**（`mysql.rs:1989` / `1721`）随函数删除消失——coalesce 下 claim 只捞 pending + 过期 processing，同批不会有他人正在处理的条目，无该竞态。

## 可选增强（默认不做）
- 若慢速编辑的 summary 重复 LLM 成本不可接受，可对 SummarySync 保留 `available_at` debounce（攒批）与 coalesce 组合——会重新引入一点复杂度，按实测成本决定。

## 验收 DoD
- 真实 MySQL 集成测试（不用 mock）：
  - 快速连续写同一文件 N 次 → Milvus chunk / summary 最终正确，embedding 调用次数 = 1（batch coalesce + watermark）
  - 跨 poll 周期慢写 → 功能正确（允许多次 LLM）
  - worker crash / lease 超时恢复正常（coalesce 不影响 lease）
- 写文件 p99 延迟应下降（事务内去掉 JSON_EXTRACT SELECT）
- 全量测试通过

## 不在本次范围（另立条目）
- lease 处理中续约（慢任务被误判 crash → dead letter 丢数据）
- 多实例 reconciler 单实例化
