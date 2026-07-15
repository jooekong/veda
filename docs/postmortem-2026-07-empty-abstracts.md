# Postmortem: 摘要 L0/L1 静默写空（2026-07-08 ~ 07-13）

- **状态**: 已修复并部署（830431c，2026-07-15 三节点）；数据已全量修复
- **影响面**: 生产 .85 全库 376 个文件摘要中 39 个 L0 为空、5 个 L1 为空；305 个目录摘要中 32 个 L0、61 个 L1 为空。伴生 315 个 PNG 孤儿任务重试至死
- **用户可见symptom**: `GET /v1/abstract/{path}` 返回 200 但 `l0_abstract` 为空串；搜索 Abstract 层无法命中这些文件

## TL;DR

生产 LLM `deepseek-v4-flash` 是推理模型（先输出思考再输出正文）。07-08~07-13 期间，airouter 网关的部分后端把思考 token 计入 `max_tokens`；veda 给 L0 摘要的预算是硬编码 150 token，常在思考阶段就耗尽——模型返回 HTTP 200 + 空 `content`。veda 的 LLM 客户端只校验 `choices` 非空，把空串当成功结果写库；`embed("")` 也成功返回单位向量，任务 completed、reconciler 对比 MySQL/Milvus 完全一致——**三层防线全部放行，一次上游行为漂移变成静默持久化的脏数据**。07-14 前后网关后端行为切回（思考不再计入），故障自愈停止，但脏数据留存直至被用户发现。

## 时间线（均为 CST）

| 时间 | 事件 |
|---|---|
| 06-09 | .85 上线即使用 `deepseek-v4-flash`，06-16/06-22 两批 46 个摘要全部正常 |
| 07-08 17:41 | 第一条空 L0 写入（`/risk/维度知识库.txt`），无人察觉 |
| 07-12 ~ 07-13 | 两批大量上传（biz-docs、1.DBPaaS 知识库），空率 ~11%（07-13 一天 299 个摘要 33 个空 L0、5 个空 L1）；同批 315 个 PNG 附件的孤儿 summary_sync 重试耗尽 dead |
| 07-14 前后 | 网关后端行为切换，空 content 停止产生（事后从重放实验反推） |
| 07-15 | 用户报告 `POST_check_70b27a.md` abstract 为空 → 调查 → 定位根因 → 修复 830431c 三节点部署 → 44 文件 + 69 目录摘要重刷归零 → 315 dead 清理 |

## 根因分析（三层）

### 1. 触发：上游 max_tokens 语义漂移（不可控）

推理模型输出分两段顺序生成：思考（`reasoning_content`）→ 正文（`content`）。`max_tokens` 的计费口径存在两种实现：

- **口径 A**（DeepSeek 官方 API 语义）：思考计入预算。L0 的 150 token 预算常在思考阶段耗尽（实测该模型思考需 60~600+ token，目录聚合可达 ~5k），此时正文一个字未写，返回 `finish_reason=length` + `content=""`，HTTP 200。
- **口径 B**：思考不计入。07-15 实测当前后端即此口径（`max_tokens=150` 时 completion 334 token 照样完整返回）。

06 月正常、07-08~13 出错、07-14 后又正常，veda 侧配置与代码在此期间零变更（config.toml mtime 06-09）——结论：**airouter 该模型的部分流量在事故窗口路由到了口径 A 的后端**，~10% 命中率与灰度/负载均衡特征吻合。L0 中招率最高因为 150 的预算最紧；L1（2048）只在思考失控的怪文件（drawio XML）上偶发中招。

> 未 100% 确证的部分：当时响应的 `finish_reason`/usage 无留存（veda 不记录），"口径 A 后端"是与全部证据吻合的最强假设。确证需 airouter 侧 07-08~13 的变更记录。

### 2. 放大：veda 三层防线全部缺失

空 content 在整条链路畅通无阻，每一层本可拦截：

1. **`llm.rs chat_once`**：只在 `choices` 数组为空时报错；`content=""` trim 后照样 `Ok("")`。对 summarize 场景空串永远非法，本应视为错误重试。
2. **`worker.rs handle_summary_sync`**：不校验生成结果，空 L0 直接 upsert 进 `veda_summaries`（status=ready）。
3. **`embed("")` 成功**：生产 embedding（text-embedding-v4）对空串不报错，返回正常 1024 维单位向量 → Milvus 写入成功 → outbox 任务 completed（实测受影响任务全部 `retry_count=0`，一次"成功"）。

### 3. 检测盲区：自愈机制的假设被打破

- reconciler 只对比"MySQL 行存在 vs Milvus 向量存在"，空摘要两边都在，永远判定一致。
- `GET /v1/abstract` 对存在的行返回 200 + 原样内容，空串不触发 202/501 任何异常态。
- 空文本的 summary 向量还会参与 Abstract 层语义检索，可能被无意义命中。

## 伴生发现：PNG 孤儿 summary_sync（315 个 dead）

同批上传暴露的独立 bug：文件先以文本态写入（疑似 FUSE/分段上传中间态，同秒内 revision 1→3），Text 路径入队 `chunk_sync + summary_sync`；随后真正的 PNG 字节覆盖写入转 blob。已入队的 summary_sync 无人撤销，worker 执行时撞 `binary blob has no text content` 的硬错误，重试 5 次至 dead。315 个 dead 全部此形态（比同期 completed 的 294 还多），污染 dead-letter 告警指标。

## 调查方法（可复用的沉淀）

1. **形态定位**：同一 summary 行 L0 空而 L1 正常（2241 字符）——两者是 `try_join!` 同时发出的请求，同模型同内容仅 max_tokens 不同 → 排除网关整体故障/内容审核，锁定预算相关。
2. **时间线统计**：按天 group by 空率，06 月两批全绿、07-08 首例、07-12/13 ~11% → 锁定窗口，排除 veda 变更（config mtime + git log 交叉）。
3. **日志反证**：事故时段 journal 干净（无 LLM 重试 warn、无 task failed）→ 证明是"HTTP 成功返回空"而非报错路径。
4. **outbox 取证**：受影响任务 `status=completed, retry_count=0` → 证明 `embed("")` 成功、无任何重试挣扎。
5. **生产重放**：在 .85 上用真实文件内容 + 逐字节复刻的 L0 prompt 直调 airouter——53 次（串行 23 + 10 路并发 30）零复现，但暴露模型是推理模型且当前后端 max_tokens 不计思考；`max_tokens=10` 探测确认截断行为存在。
6. **反事实验证**：`embed([""])` 实测成功返回单位向量，闭合"为什么任务能 completed"的最后一环。

## 修复与恢复

| 项 | 内容 |
|---|---|
| 代码（830431c） | L0/L1/目录聚合共用 `max_summary_tokens` 预算，删除两处硬编码 150；默认 2048 → 8192（8192 为几乎所有 OpenAI 兼容后端都接受的安全上限；口径 A 下思考+正文亦富余；max_tokens 是上限非实付，成本不变） |
| 部署 | 07-15 按 runbook 三节点（.161/.89/.85）同 binary `8b0e6080`，全清单验证绿，观察 40 分钟无异常 |
| 数据 | 44 条文件空摘要 + 69 条目录空摘要重新入队重刷，终态 file 376 / dir 305 全库双零，零 dead；315 条 PNG dead 任务删除（删前备份） |
| 运维 | .85 `TimeoutStopSec` 10s → 120s（对齐部署模板，保护 90s deadline 的 answer 长请求） |

## 经验教训

**做得好**：
- outbox 任务留存了 `retry_count`/status 取证痕迹；journal 保留窗口足够长。
- 修复前先在生产实测了 8192 对全部四种 prompt 形态的行为（15 次真实请求全过），而非拍脑袋改参数。

**做得不好**：
- "LLM 返回什么都算成功"的隐含假设从未被审视——对生成式依赖，**输出合法性校验（非空、格式）应当与 HTTP 状态码校验同等对待**。
- 摘要质量无任何指标：空摘要写了 7 天、39 个文件，靠用户翻文件才发现。`veda_llm_total{outcome}` 只统计 HTTP 结果，空 content 计入 ok。
- 对推理模型的预算敏感性缺乏认知：150 token 的预算在 2026 年的模型生态里是危险值。

**运气好的部分**：
- 上游 07-14 自行切回，未持续恶化。
- embedding 对空串返回的是固定单位向量而非报错——虽然是防线缺失的一环，但也意味着没有产生半写入的脏状态，重刷即愈。

## 行动项

| # | 项 | 状态 |
|---|---|---|
| 1 | max_tokens 预算修复 + 部署 + 数据重刷 | ✅ done（830431c） |
| 2 | `chat_once` 空 content 视为 retryable 错误（根本兜底，防上游任意怪行为） | 待做 |
| 3 | `handle_summary_sync` 对 Blob 文件优雅跳过（消除 PNG 孤儿 dead 复发） | 待做 |
| 4 | `detect_output_language` 跳过 YAML frontmatter 采样（中文文档摘要被判英文） | 待评估 |
| 5 | airouter 侧确认 07-08~13 后端变更（需平台组配合） | 可选 |
