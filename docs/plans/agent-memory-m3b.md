# Agent/团队记忆 M3b — qa_log 自动摄入

> 施工图。架构定稿 [`../design/agent-memory.md`](../design/agent-memory.md)
> §3/§13/§16（M3 摄入规则）；身份前置在 [`agent-memory-m3a.md`](agent-memory-m3a.md)
> （操作者解析 `wecom:<userid>` → principal）。**执行顺序在 M4a（浏览页）之后**——
> 浏览页是摄入质量的质检面，先有质检面再开自动写路径（顺序对调的完整理由见 m4a §0）。

## 0. 背景与目标

- 设计 §16 定死的摄入规则：**自动抽取的记忆一律落提问人的个人域，团队域永远
  只收显式写入**。M3a 落了 wecom 身份解析，摄入的身份前置就绪。
- 目标场景（design §2.1 排序第 2 的纠错闭环）：有人在企微里纠正 bot
  「不对，正确口径是 Y」→ 自动记进**他自己**的个人域 → 他下次私聊提问，
  answer 三域注入带出这条。团队化靠他显式 `scope=team` 或 M3c 收敛提名，
  自动管线不生产团队事实。
- **抽取口径收窄（偏离 design §3 初稿的「读 episodes 抽原子事实」宽口径，
  2026-08-20 Joe 拍板）**：**只抽用户断言，不抽 bot 答案内容**。bot 的答案来自
  文档检索，answer 路径本来就会再检索到文档——把答案内容复写进个人记忆是
  doc-echo，纯噪音，还挤占注入名额。有价值的只有两类：提问里自带的事实/偏好
  （「我们组的 collector 地址是 X」）、对 bot 的纠错（「不对，应该是 Y」）。

## 1. 范围（三项）

1. **摄入任务**（veda-server 进程内新模块 `memory_ingest.rs`，main.rs 里
   retention sweep 同款 spawn + interval 循环）：
   - **读源**：直读 `veda_tunnel_qa_log`（server 与 tunnel 同库，
     `tunnel_bots.rs` 先例「进程间零 RPC」；tunnel 零改动，保持纯生产者）。
     水位之后的新行，按 `(bot_id, chat_key)` 分组成会话窗；全 outcome 都进窗
     （error/no_context 行的 query 也可能含用户断言，不加筛选分支）。
   - **窗口喂 LLM**：窗内按序渲染 `提问人: query` + `bot: answer_text 截断摘要`
     （答案只作上下文供识别纠错，prompt 明说不从中抽事实）。输出 JSON
     0..k 条 `{content, kind(fact|preference|procedure), user_id, qa_log_ids}`。
     prompt 固化五条：只抽用户断言的新信息、宁缺毋滥拿不准不抽、每条独立可懂、
     能合并先合并、明确允许输出空（腾讯三原则 + 窄口径，design §7 同源）。
     机械校验：user_id 必须在窗口内出现、qa_log_ids ⊆ 窗口行，不合规整条丢。
   - **落库**：bot 行的 `veda_key` 经既有 key 鉴权路径解析 workspace（key 无效/
     吊销 → 跳过该 bot 的行并 warn）；`resolve_operator_actor(Wecom, user_id)`
     解析提问人；走 `MemoryService::save`，scope=Mine、origin 走 kind 默认
     （fact/procedure 锁 W，正确语义）、`source_ref={"qa_log":[ids]}`。
     `SaveMemoryInput` 加内部字段 `skip_if_similar: Option<f32>`（REST/MCP DTO
     不暴露）：save 本就先算近邻，自动路径 top-1 cos ≥ 0.90 → 跳过不写
     （常量不进配置，dogfood 后再议）；精确重复由 content_hash 唯一键幂等兜底。
   - **降级**：新身份 + 目录不可用 → 该条跳过（M3a 规则：不造半身份），
     warn 后继续本窗其余条目，**不重试**——qa 持续流动，同类信息会再来，
     为丢一条候选建行级重试队列不值。LLM 失败/JSON 坏 → 整窗跳过、
     水位不推进、下轮重试（30min 周期即退避，不加专门退避逻辑）。
   - **水位**：新表 `veda_memory_ingest_state(source VARCHAR PK, watermark_id
     BIGINT, updated_at)`，source='qa_log' 单行；乐观更新
     `WHERE watermark_id = 旧值`。测试环境双实例并发最坏重复一轮抽取，
     结果幂等由 content_hash + skip_if_similar 两层兜住，LLM 白烧一次可忽略。
   - **上限**：每轮每窗 ≤50 行、总量 ≤500 行，超出水位停在实际处理位置下轮续。
   - **埋点**：obs 计数器 抽取/跳过(近邻)/跳过(降级)/整窗失败。
2. **配置**：`[memory_ingest] enabled(默认 false) / interval_secs(默认 1800)`。
   硬依赖 `[llm]`（未配则任务不 spawn，启动 info 一条）。**默认关，测试环境
   先开 dogfood，浏览页抽查质量满意后再翻生产**——这正是 M4a 先行的原因。
   embedding 走既有后台低优先闸（交互流量不受影响）。
3. **抽取 prompt 常量**：中文，随代码固化（不进配置）；核心句子如 §1.1。

## 2. 设计决定

- **server 进程内直读，不走 REST**：in-process 调 MemoryService 复用 embed 闸/
  outbox 自愈/去重全链路；tunnel 保持「veda 数据面标准消费者」不长后台任务。
- **只抽用户断言**：偏离 design §3 宽口径的拍板记录在 §0；doc-echo 论证同上。
  bot 答案只作纠错识别的上下文。
- **群聊私聊同规则**：都落提问人个人域，代码零分支。受众规则（M3a §2）管的是
  **注入**面；摄入面用户在群里公开说的话落他自己的域，无新增暴露面。
- **点踩数据不进记忆**：那是文档缺口清单（qa_log bad-case 面板已有），
  修文档不修记忆，两条线不串。
- **单 LLM 单 pass，不做抽取复核**：质量闸 = prompt 宁缺毋滥 + 机械校验 +
  浏览页人肉抽查；二级复核 LLM 等有质量证据再说。
- **expires_at 不自动设**：写入时知道保质期才填（design §9），LLM 猜保质期
  是反模式。
- **丢行取舍**：目录降级跳过的行不重试（简化，接受丢候选）；这是 M3a
  「新身份 + 目录挂 → 按无操作者处理」在摄入面的同一条规则。

## 3. 测试（真实 MySQL/Milvus/embedding/LLM，`--ignored`）

- 摄入 e2e：种 qa 窗——一条用户纠错（「不对，X 的正确口径是 Y」）+ 两条纯文档
  问答 → 跑一轮 → 断言：纠错抽出记忆落**提问人** mine 域、source_ref 带
  qa_log ids、纯文档行抽出 0 条（**doc-echo 阴性断言**）；再跑一轮 0 新增（幂等）。
- 闭环 e2e：抽出后，该提问人带 `wecom:` 操作者头的 context/answer 召回该记忆
  （citation scope=个人）。
- 单测：水位乐观并发（两实例同推进只成一个）、窗口分组切分、LLM 输出机械校验
  （user_id 越界丢弃 / JSON 坏整窗跳过）、bot veda_key 无效跳过、
  skip_if_similar 阈值路径、`[llm]` 缺席不 spawn。
- e2e 语料进 `docs/testing/sop-fixtures/`（强信号纠错句，抗 LLM 输出抖动）。

## 4. 文档

ARCHITECTURE.md（memory 节 + veda-server 后台任务清单）/ CHANGELOG.md /
`config/server.toml.example`（`[memory_ingest]` 块）/ deploy-runbook
（灰度开关一句：测试环境先开）/ design doc 状态行。无新 API 端点，api 文档不动。

## 5. 不做（防散）

对账提名 + 收敛提名（M3c，共用提名表到时一起设计）；digest/画像（触发条件
不变）；bot 答案内容抽取；点踩→记忆；群聊/私聊差异化处理；二级复核 LLM；
行级重试队列 / 专门退避；阈值与窗口参数进配置（常量，dogfood 后再议）；
ai-db-bridge 查询记录源（design §19，单独一轮讨论）；手动触发摄入的
REST/MCP 端点（纯后台）；tunnel 侧任何改动。

## 6. DoD

全部 e2e 绿 + 测试环境部署且 `enabled=true` 跑满一周 dogfood + 用 M4a 浏览页
抽查抽取质量（噪音条目人肉删得过来、doc-echo 不出现）+ 真实冒烟：企微私聊
纠正 bot 一句 → 下轮摄入后同人再问，answer 引用到该记忆；归档本 plan 并更新索引。
