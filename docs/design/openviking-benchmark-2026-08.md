# OpenViking 对标（2026-08-19）：memory / session / MCP recall

> 对标对象：`volcengine/OpenViking`，字节火山引擎的 "Context Database for AI Agents"。
> 基线：HEAD `dc39985`（2026-08-19 17:54），full clone 2036 commits（回溯至 2026-01-29），最新 tag `v0.4.15`。
> 方法：源码级 fan-out（6 路 subagent + 主线交叉验证），凡本文标「已验证」的结论均由主线独立 grep/读码复核过。
> 上一轮对标见 [`agent-memory.md` §18](agent-memory.md)（2026-08-07/08-11），当时只记了一行「`viking://` 文件系统心智 + 写入时 L0/L1/L2 分层，摘要无校验」。本文是深挖。

---

## 0. 一句话结论

OpenViking 这三个月的主线是 **session → memory 的自动抽取管线** 和 **服务端 context 组装（原 recall）**。
前者工程量巨大但奖励信号是退化的；后者（预算 / 分层 / 降级 / 去重 / 引用校验）扎实、便宜、**veda 应该逐条抄**。
它最响亮的宣传点 "observable retrieval trajectory" 基本没实现。
而 veda 当初拍板「记忆是表不是文件」「不做图」「不做写时 LLM 决策」，这轮对标给出了三条独立的实证支持。

---

## 1. Memory 子系统

### 1.1 memory type = YAML schema，不是枚举

一个 YAML 文件同时定义四件事：存储路径、prompt 语义、LLM 结构化输出 schema、字段合并策略。
加载三层覆盖：bundled → `experimental_memory/`（flag）→ `custom_templates_dir`。放一个 YAML 进去就是一个新 memory type，不改代码。

`MemoryTypeSchema` 关键字段（`session/memory/dataclass.py:190-213`）：
`memory_type` / `description`（type 级 prompt）/ `fields[]`（每个字段的 `description` 就是字段级 prompt）/ `filename_template` / `content_template` / **`embedding_template`** / `directory` / `operation_mode`(`upsert|add_only|update_only`) / `stage`(`user|agent`) / `peer_enabled` / `overview_template`。

内置 11 种：`profile` `preferences` `entities` `events` `identity` `soul` `cases` `trajectories` `experiences`（enabled）+ `skills` `tools`（**`enabled: false`**，`ed1bd4b8` 关掉，commit message 首行就是 `disable unsupported tool and skill extraction`）。

字段级 `merge_op` 四种：`patch`（SEARCH/REPLACE）/ `replace` / `sum` / `immutable`。**merge_op 同时决定了 LLM 看到的输出类型**（`str` vs `StrPatch`）和落盘合并逻辑 —— 一处改动两头生效，这是整套设计里最漂亮的一点。

### 1.2 存储：记忆就是一个 Markdown 文件

```
viking://user/{uid}/memories/profile.md
viking://user/{uid}/memories/entities/{category}/{name}.md
viking://user/{uid}/memories/events/2026/08/19/{event}.md
viking://user/{uid}/peers/{peer_id}/memories/...        ← peer 作用域
viking://user/{uid}/memories/{type}/.overview.md         ← L1 sidecar
```

没有 ID，**URI 就是主键**。元数据以 `<!-- MEMORY_FIELDS {json} -->` trailer 追加在正文末尾。版本是单调 int，无历史快照（experiences 例外，走 git commit）。

向量索引是派生副本。**memory 记录的 `abstract` scalar 存的是去掉 link 的完整正文（上限 50KB）且兼作 embedding 文本**（`memory_updater.py:1394-1401`）—— 这条事实驱动了整个 tier 系统（见 §3.4）。

### 1.3 写入：两级 LLM + 一级确定性 merge

第一级确定性：按字段 `merge_op` 应用。`apply_str_patch` 命中 >1 次**直接抛错**让 LLM 带更多上下文重生成（`80d63f50` 修的就是原来无 count 的 `.replace()` 会把所有匹配位置一起改掉）。

第二级 LLM：`StreamingMemoryUpdater` 按 `(peer_id, memory_type)` 分组批处理（`max_items=8, max_wait=10.0s`），**同一组里只要有 2 条 patch 就必然再来一次 LLM** 做语义去重和目录名归一化（`activity` vs `activities`）。

关键防线：**LLM 的路由输出永远是提示，不是权威**。三处独立覆写 —— `fill_identity_fields` 无条件覆写 `user_id`；`calculate_memory_uris` 歧义即返回 `[]` 绝不猜；`enforce_merge_group_peer_id` 用 group key 重写 merge LLM 的 `peer_id` 并重算 URI（注释直说 "The second-stage merge LLM may omit or hallucinate peer_id"）。

### 1.4 lifecycle：几乎没做

`retrieve/memory_lifecycle.py` **整个文件 64 行，一个纯函数**，2026-02-26 出生后除 license header 零改动：

```
score = sigmoid(log1p(active_count)) × exp(-ln2/7d × age_days)
```

两个消费者：检索加权（`hotness_alpha`，**默认 0.0 = 关闭**，已验证）和健康度看板。
**没有 TTL、没有遗忘、没有 eviction、没有 promotion。**
`examples/ov.conf.example` 里的 `enable_memory_decay` / `memory_decay_check_interval` 两个键**在代码里根本不存在**，而配置模型是 `extra="forbid"` —— 照示例配置会直接启动失败。

### 1.5 隔离：这块做得扎实

三道确定性闸，**不依赖 prompt 或 ranking**：路径映射到 `/local/{account_id}/`（跨账号物理不可达）→ URI 级 ACL → 向量检索恒加 `Eq("account_id")` + `PathScope(visible_roots)`。

代价也记下来了：引入 User/Peer 隔离模型的 `ff258768`（445 files / −12230 行）之后 **2.5 个月内至少 9 个修复 + 一次 4413 行迁移**。veda 的 emp/dept 域若要扩到「代表别人写记忆」，这条尾巴要预算进去。

---

## 2. Session 子系统

### 2.1 两阶段 commit（**veda 最该抄的工程设计**）

Phase 1 同步、全程持树锁（30s 超时），顺序严格：

```
1 acquire lock → 2 严格重读 messages.jsonl → 3 重读 meta → 4 探测 archive_NNN
5 写 phase1 intent marker{status:"preparing", archived/retained message ids}
6 写 archive/messages.jsonl → 7 enqueue → 8 create task
9 重写 root messages.jsonl = retained → 10 save meta → 11 marker status="ready"
```

崩溃恢复不靠事务日志，**靠权威文件的前缀比对**判断第 9 步是否真落盘：

```python
phase1_applied = live_ids[:len(retained_ids)] == retained_ids \
                 and not (set(archived_ids) - set(retained_ids)).intersection(live_ids)
```

archive 状态机用 marker 文件：`.done` / `.failed.json` / 无（pending）。上下文装配 newest→oldest **扫到第一个终态就停** —— archive 无限增长，装配成本 O(1)。

### 2.2 抽取：case-gated cascade

`SessionCompressorV3.extract_long_term_memories` 是唯一入口。**没有 case 就没有 trajectory，没有 trajectory 就没有 experience，也没有 skill** —— 整条"自进化"链挂在"这一轮普通抽取是否吐出至少一个 `cases` 文件"上。

自动 commit **默认全关**（`default_enabled=false` / `idle_enabled=false`），所有实际的自动 commit 都是各 harness 插件自己实现的客户端阈值 —— 进程被 `kill -9` 就没有任何集成会 commit。

### 2.3 Working Memory v2 的服务端确定性护栏（**第二值得抄**）

七段：`Session Title / Current State / Task & Goals / Key Facts & Decisions / Files & Context / Errors & Corrections / Open Issues`。
LLM 每段返回 `KEEP / UPDATE / APPEND`，**服务端用规则强制**（配 81 个单测）：

| 护栏 | 规则 |
|---|---|
| 段大小预警 | bullets > 25 或 est_tokens > 1500 → prompt 里插 `<section_size_warnings>` |
| APPEND-only 段 | `Errors & Corrections` 的 UPDATE 被降级为「只 APPEND 老正文没有的 item」 |
| 反瘦身 | Key Facts 的 UPDATE bullet 数 < 老的 15% 或词法锚点覆盖 < 70% → 拒绝，但抢救出真新增走 APPEND |
| 反膨胀 | 超阈值 1~2 倍 → 只收去重后前 5 条；超 2 倍 → 全拒 + 插幂等 sentinel `[⚠ CONSOLIDATION REQUIRED]`，下轮 sentinel 已在则彻底 KEEP（硬停） |
| 路径不许消失 | Files 段 UPDATE 少了任何一条路径 token 就拒，改 APPEND |
| 标题稳定 | 新旧标题实词交集为 0 → 判 drift，KEEP |
| 静默丢弃恢复 | Open Issues 老 bullet 找不到 → 追加回去标 `[silently dropped, restored]`，只恢复一次 |

**思路：不试图靠 prompt 让模型别忘事，在服务端用确定性规则兜住。**

### 2.4 tool result 外置 + 零 LLM synopsis（**第三值得抄**）

超 `threshold_chars=20000` 的工具输出外置到文件，会话里留 typed stub。
`tool_result_synopsis.py` **完全不调模型**：嗅探 kind ∈ `{json,csv,tsv,yaml,xml,code,text,unknown}`，按类型结构采样（JSON 深度 ≤2 / 数组样本 3 / 对象 key 10；YAML key 30；XML 子标签 30；code 走正则抓 import ≤12、符号 ≤24；text 取头 18 行 / 首尾各 500 字符）。
stub 末尾有 `Explore:` 三行显式指路，告诉 agent 怎么把原文捞回来。
内容寻址 `tr_{tool_id}_{sha256[:16]}`；**agent 读回原文再入库时改写成同一 source 的引用**，挡住 transcript 平方级膨胀。

### 2.5 "self-evolving" 祛魅（已验证）

`session/train/` 是 TextGrad 式文本策略优化：`experiences/` 目录 = policy set，`PatchSemanticGradient` = 一份 LLM 改写后的完整 markdown 文件（不是数值、不是 diff）。一次训练步三次独立 LLM 调用。

**但生产路径的 reward 是退化的**（`trajectory_analyzer.py:379`，已验证）：

```python
passed = bool(trajectories); score = 1.0 if passed else 0.0
```

全仓**没有任何 `RolloutEvaluator` 实现**，只有 Protocol 和一个测试 fake。`Case.rubric` 被 LLM 提取并持久化，在线路径上从不评估。
`confidence` 算了（四行启发式），**下游从不做阈值判断** —— 0.1 和 0.9 一视同仁地被应用。

闭环放大风险结构上成立：写入无 reward gate + confidence 无阈值 + optimizer 可自由改写/删除 policy + experiences 目录又是下一轮的检索源。唯一刹车是 base-content 相等检查（防丢更新，不防坏更新）。缓解因素：两扇门默认都关。

---

## 3. MCP + recall（**Joe 点名的重点**）

### 3.1 `recall` 工具已删除，折进 `search(mode="context")`

`eb5aaf78` / PR #4075，2026-08-18（已验证：`mcp_endpoint.py` 里 `async def recall` 零命中）。
当前 15 个 tool：`find search read list tree remember write edit add_resource list_watches cancel_watch grep glob forget health`。

`search` 一个工具 21 个参数，`mode="context"` 返回**注入就绪、按 token 预算裁剪好的上下文块**：
`query_expansion` / `max_tokens=1600` / `quotas` / `purpose∈{chat,coding}` / `detail` / `detail_by_category` / `dedup_turns` / `exclude_uris` / `peer_scope` / `other_peer_penalty(ies)` / `rewrite` / `rewrite_max_bullets`。

顺带一条对照：**OpenViking 15 个 tool 零 annotation**（`readOnlyHint`/`destructiveHint` 全仓 grep 不到），包括不可逆的 `forget`。veda 的 `readOnlyHint` 07-30 已上三节点，这项 veda 领先。

### 3.2 recall 全链路（严格串行八阶段）

```
① 参数归一化（quotas / penalties）
② intent gate + session 加载
③ query expansion（LLM #1，5s 熔断，失败退回原 query）
④ ledger 加载 → cooled_uris ∪ exclude_uris(≤200)
⑤ gather ← 唯一打向量库的阶段
⑥ 按需读正文（Semaphore=8；默认路径下绝大多数候选零读取）
⑦ budget 规划
⑧ render + digest（LLM #2，30s 熔断）
```

### 3.3 token budget 算法（核心，约 110 行纯逻辑）

```
per_entry_cap = max(1, max_tokens // N * 2)          # 平均份额的两倍
```

**Floor pass（广度优先）**：按分数降序遍历**全部**候选，每条从起始 tier 逐级**向下降级**找第一个装得下的（降到底是 bare URI）。设计理由写在注释里：*"Scores cluster in a narrow band, so spending the whole budget on the top hit is a bad bet."*

**Depth passes**：`overview` → `full` → `full(无 cap)` 三轮升级。

**关键不变量：超预算降级，绝不截断** —— 半截 markdown 对 LLM 比一个 URI 更糟。

token 估算是 **CJK-aware** 的：CJK/全角 = 1.5、`>0xFFFF` = 2.0、其余 = 0.25，计费对象是渲染后含标签的整段 fragment。

### 3.4 分层：搜什么 vs 返回什么，彻底解耦

- **recall gather 恒 `level=None`**（`gather.py:287`）—— L0/L1/L2 全部参与召回
- `detail`/tier 只决定**返回粒度**
- `mode="context"` 里传 `level` 直接被忽略并记进 `stats["ignored"]`

memory 类的 `abstract` = 全文（零读取），所以对 memory 而言 `overview`（只抽 `# Summary` 段）是**更便宜的降级**而非升级；resources/skills 的 `abstract` 是 256 字摘要，降级方向是 bare URI。注释还写了退出路径：一旦 writer 存了独立的 summary scalar，events 就搬回 abstract。

### 3.5 recall 的两个真护栏

**digest 引用白名单强校验**（`rewrite.py:48-65`）：每条 bullet 先截断到 500 字符**再**提取 URI（理由：URI 在句尾，被截掉就等于没引用），然后对照本轮实际返回的 entry URI 白名单，越界整条丢弃，全丢则回落未改写文本。

**`<memory>` 标签转义**（`render.py:22-32`）：正文里的 `<memory` / `</memory` 被替换成 `<\memory`，防止一条记忆伪造出带自定义 `uri`/`score` 的兄弟节点。刻意**不整体转义 body**，因为 agent 按 markdown 读。

### 3.6 stats block —— 真正可用的"轨迹"

```json
{"searched":{"events":12,...}, "candidates":13, "excluded":2,
 "origins":{"self":13,...}, "max_tokens":1600, "used_tokens":1510,
 "per_entry_cap":320, "returned":13, "dropped":0, "deduped":0,
 "tier_counts":{"abstract":11,"overview":2},
 "fill":{"floor_tokens":1490,"overview_upgrades":0,"full_upgrades":1},
 "query_expansion":"used", "planned_queries":[...], "rewrite":"ok",
 "dedup":{"turns":5,"status":"ok","cooled":2,"turn":34}}
```

每一级过滤的「进多少出多少」都记下来了。讽刺的是它不叫 trajectory，而**真叫 trajectory 的那个是空的**。

### 3.7 宣传 vs 实现（已验证）

README/FAQ 卖的 "Every retrieval leaves a trajectory you can watch and debug"：
`ThinkingTrace` 定义了 10 种事件类型、线程安全队列、统计聚合，**`add_event()` 在检索路径一次都没被调用**（`retrieve/`、`storage/`、`server/` 全部零命中）。`searched_directories` 返回的是规划起点而非实际走过的路；`match_reason` 恒空。

另外两条：`score_propagation_alpha` 默认 1.0 = 目录递归的分数传播**被关掉**（递归只决定扫哪些子节点，不影响排序）；`find` 的行为被 rerank 配置**隐式切换**（有 rerank 才走目录递归，否则单发扁平搜索）。

---

## 4. 被放弃的方向（信噪比最高）

| 方向 | 结局 |
|---|---|
| **resource relation 图边** | 两步死亡：2026-06-10 检索侧先把 `relations` 摘成恒空（`27e90ad9`），**拖了 10 周**，08-19 才连同 3 个 SDK、CLI、双语文档一起铲掉（`dc39985`，109 文件 / **−2449 行**）。幸存的同源机制是一个 `active_count` 整型标量 |
| **memory link / page_id 图** | 造了 3 个月，`link_enabled` **默认 False 从没翻过**（已验证），设计文档仍 Draft，`graph_view.py` 688 行**零调用方** |
| **self/peer 独立的 memory type 过滤器** | 合入 **88 秒**后被整体 revert（`6fc4d8f` → `afa5aae`）。重来版（open PR #4126）明确砍掉 per-target 过滤，退回单一 allowlist |
| **自建多语言 AST 栈** | 9 种语言手写提取器，10 周后换成 `grep-ast` 一个 pip 依赖（−6305 行） |
| **memory 文件多版本 / `data_version` time-travel** | RFC #2277 写得很完整，**从未实现**。最终用「内容哈希快照 + read 消费记录」近似 |
| **memory 抽取 v1（四段式 LLM 流水线）** | 抽取 → LLM 去重决策 → LLM 合并 → LLM 字段压缩，5249 行整体删除 |

一代抽取管线活 4–5 个月。三次 revert 分别在 88 秒 / 23 分钟 / 当天 —— trunk-based，坏了先 revert 再重来。

---

## 5. 对 veda 的启示

### 5.1 值得抄（按 ROI 排序）

| # | 抄什么 | 为什么 | 成本 |
|---|---|---|---|
| **1** | **`detail_level` 解耦：召回不限层，层只决定返回粒度** | veda `search.rs:123` 分派到 `search_abstract`/`search_overview`/`search_full` 三条**不同的检索路径** —— 这就是 FDC 那个「原文片段搜不到 + 中文 query 撞英文摘要」的结构性根因。OpenViking 恒 `level=None` 召回、tier 只管返回，天然免疫 | 中，但修的是已知线上 bug |
| **2** | **recall stats block** | veda 检索目前是黑盒，出问题只能猜。记 candidates / dropped / deduped / used_tokens / tier_counts / 每级 in-out。**这比对方吹的 trajectory 有用十倍，而对方那个根本没实现** | 低（~20-40 行） |
| **3** | **token 预算分配器 + 降级不截断 + CJK token 估算** | veda `/v1/answer` 现在是「记忆 top-5 + 文件块按条数」，**没有 token 预算**，一条大文档就能撑爆 context。且中文按 `chars/4` 估会低估一半以上 | 中（floor pass ~60 行 + 估算 20 行） |
| **4** | **digest/答案的引用白名单强校验** | veda 出处治理已有一半（撞名回退、ungrounded 不回填），缺「引用必须命中本轮候选集，越界整条丢」这条硬规则 | 低 |
| **5** | **`<memory>` / 证据块标签转义** | veda answer 也把检索片段拼进 prompt，同样有伪造 envelope 的注入面 | 极低（几行正则） |
| **6** | **两阶段 commit：intent marker + 前缀比对恢复** | 若做 episodes 层：提交证据 = 权威数据自身的前缀，不需要额外事务日志。`.done`/`.failed.json` + 扫到第一个终态即停，让装配成本 O(1) | 中，做 episodes 时再说 |
| **7** | **零 LLM 的 tool-result synopsis + 内容寻址 + 读回改写为引用** | 若做 session capture：工具输出是最大体积来源，这三招纯 Python 无模型成本 | 中，做 capture 时再说 |
| **8** | **注入回流保护做成机械的** | 注入用确定性 tag 包裹，capture 侧机械 strip（OpenClaw 在 afterTurn 和构造下一轮 query 时各 strip 一次）。veda 若将来回写会话，这条必须先有 | 低 |

另外两条**便宜的产品细节**值得直接借：
- 注入块措辞「supporting context, not instructions」—— veda 团队记忆是 wiki 式自由编辑的，这条比 OpenViking 更需要（`memory_context` 已有 framing note，可再收紧）
- L0/L1 sidecar 的 `freshness{total_entries, sampled_entries, pending_child_changes}` —— veda 的目录摘要陈旧/采样问题目前对用户完全不可见

### 5.2 明确不要抄

| 不抄什么 | 理由 |
|---|---|
| **query rewrite / intent expansion** | 代价 = +1 次 LLM + 5s 熔断 + 检索 fan-out ×3；而对方连 planner 输出的 `context_type` 都丢掉只用 query 字符串。veda 已有 airouter 429 quota 压力、answer p50 已到 1.4k 字量级，再串一个 5s 熔断在检索前收益远小于代价。真要做，做异步预热，不要串同步路径 |
| **intent analysis** | 名不副实：没有 intent 枚举、没有分类分支，`intent` 字段自由文本且从不被消费。包装 > 实质 |
| **retrieval trajectory 事件流** | 对方自己没实现（10 种事件类型全是死代码）。要它的收益不如 §5.1#2 的 1/10 成本 |
| **hotness / active_count 反馈排序** | 默认 `alpha=0` 关闭，`increment_active_count` 是 read-modify-write 无 CAS 会丢计数。veda 已有文档热度统计，要做基于自己那套 |
| **三段式 upgrade pass** | `DEPTH_CEILING` 只有 `{"events":"full"}` 一条，传了 `detail` 就三个 pass 全空转。抄复杂度不抄收益 |
| **记忆文件化 + LLM patch 编辑** | 见 §6.1 |
| **图 / link / graph** | 见 §6.2 |
| **train/ 自进化** | reward = `bool(trajectories)`，无 evaluator，confidence 无阈值。若 veda 做自动抽取，要么接真评估，要么保持「提议—人工确认」，不要直接写库 |
| **把 memory 全文塞进向量库 payload 省一次回查** | OpenViking 这么做了（abstract=全文 50KB）。**veda 不能抄** —— MySQL 复核是 GateMem 两指标确定性归零的根基，省这一次查询等于拆掉地基 |

### 5.3 veda 做对了、这轮对标给出实证的

1. **记忆是表不是文件**（§6.1）—— 三篇 superpowers spec 全在处理文件路线的税
2. **不做图**（§6.2）—— 三重独立印证
3. **不做写时 confidence / LLM 决策** —— 对方 confidence 算了从不设阈值，reward 退化成布尔
4. **分域硬隔离** —— 对方 `peer_scope` 默认 `"all"`（跨 peer 可见，只降权 0.02~0.1 不隔离），且 workspace peer 从 `cwd` 派生（共享 runner 上 A 的记忆会进 B 的会话，官方自己在 README 末尾警告，但默认值就是不安全那档）
5. **agent 显式写一行记忆** —— 对方 `remember` 只是「投素材 + 触发抽取」，内容不可控、成本无上界、失败静默（抽取解析不出 JSON 时降级为「没有 operations」且 commit 仍报成功）
6. **MCP annotations** —— 对方 15 个 tool 零 annotation 含 `forget`
7. **outbox + dead-letter + 三次重试** —— 对方无 dead-letter，transient 失败无限 requeue，无 attempt 计数无退避
8. **服务端兜底而非客户端阈值** —— 对方所有自动 commit 都在客户端插件里，服务端默认全关

---

## 6. veda 哪些设计不合理

分三类：**真问题（该修）/ 取舍（知道就行）/ 现存缺陷（顺手清）**。

### 6.1 真问题

**T1. `detail_level` 耦合了「搜哪层」和「返回什么」**（`veda-core/src/service/search.rs:123`）
三条独立检索路径意味着 `detail_level=abstract` 时**只在摘要层做向量检索**，原文片段永远召不回，中文 query 撞英文摘要还会跨语言错位 —— 这正是 08-11 FDC 诊断的根因，当时定性为「修复方向待拍板」。
对标给出了正解：**召回层不做 level 过滤，`detail_level` 只决定返回粒度**。这条同时解掉「dir 摘要 prefix 恒丢」那一类问题的一半。

**T2. `/v1/answer` 证据流没有 token 预算**
现状是 `MEMORY_INJECT_LIMIT=5` 条 + 文件块按条数，**按条数不按 token**。一条大文档块就能吃掉大部分 context，而记忆块编号在文件块之前 —— 挤掉的是后面的文件证据。
应做：breadth-first floor（每条先拿最便宜的粒度）→ 剩余预算再加深 → **超预算降级不截断**。附带需要 CJK-aware 的 token 估算。

**T3. 检索链路零可观测**
没有 stats、没有每级过滤计数。线上出「搜不到」只能靠人肉复现（FDC 那次就是）。§5.1#2 的 stats block 是最低成本的补救。

**T4. `last_used_at` 只写不读**（`service/memory.rs:485-488`）
每次检索后一次额外 UPDATE，**零读取方**。设计文档 §8/§9.1 的「排序下沉」和 M1 plan 的 `similarity × recency × kind` 都没实现。
两条路选一条：接上排序（mem0 那套 0.3–1.5× 缩放只调序不扩召回，成本极低），或者删掉这次写入。**现状是纯负债**。

**T5. `topic` 列写入后零消费者**（`schema.rs`、`service/memory.rs:284-289`）
还专门实现了「继承 top-1 近邻 topic，阈值 0.75」的逻辑来填它。设计文档 §10.4 自己警告过「摆设列」，现在就是。digest 层不做的话，这列和它的继承逻辑应该一起删。

**T6. 过期记忆永不物理删除 + memory 无 reconcile**
`expires_at` 只在读时 SQL 过滤，`RetentionConfig` 只扫 `veda_fs_events` 和 `veda_outbox` —— 过期行和它们的 Milvus 向量无限累积。
且 memory 没有 reconcile 兜底（M1 plan 列为「可选」），outbox 三次重试 dead 后就是永久漏召回，**而 `handle_memory_sync` 零单测，worker 能否真治愈向量从未被验证过**。

### 6.2 取舍（知道就行，不必动）

- **RRF `k=60` 无调参无测试**，5× 超采、sparse 不带 metricType、Strong consistency、no-fallback 全无断言。hybrid 只有 1 条 e2e（`XK9Q7Z` 罕见 token），只测召回不测排名。对标对象在这块更弱（融合是 VikingDB 服务端一个标量，默认 0.0 纯 dense），所以不急。
- **每请求至少 2 次 principal 查询**（无条件先 `resolve_key_actor` 再 `resolve_operator_actor`），注释说「本就要 ensure，无额外查询」，实际每请求都打。量级无感。
- **缺索引**：只有 `uq_scope_hash` 和 `idx_scope_topic`，无 `created_by`/`last_used_at`/`expires_at` 索引。M4 浏览页一上就是全表 filesort —— 到时候再加。
- **memory_sync 无 outbox dedup**，高频编辑同一条会堆多行 pending。fs 侧有 `try_insert_outbox_for_file`，memory 没复用。

### 6.3 现存缺陷（已交叉验证，顺手清）

| # | 缺陷 | 位置 |
|---|---|---|
| 1 | **`mcp_http_test.rs` 断言 7 个工具名，实际 12 个** —— 跑起来就红（该 suite `#[ignore]` 所以平时不暴露） | `crates/veda-server/tests/mcp_http_test.rs:459` |
| 2 | **`memory_context` 解析出 `scope` 但 handler 不用**，schema 里也没声明 —— 客户端传了既不生效也不报错 | `routes/mcp.rs:1233-1238` + `MemoryQueryArgs` |
| 3 | **`memory_save` 的 `kind` enum 漏了 `derived`**，但 service 接受 —— advertised schema 与实际接受面不一致 | `routes/mcp.rs:531-533` |
| 4 | `memory_delete` 无 DTO、无 `deny_unknown_fields`，多余参数静默忽略（其余四个都有） | `routes/mcp.rs:1175-1178` |
| 5 | `mcp.rs:1032` 模块注释仍写 `Identity = the wk_ key (M1).`，M3a 后已不成立 | `routes/mcp.rs:1032` |
| 6 | **文档 drift**：`plans.md:25` 写 M3a「待实施」、`agent-memory.md:5` 写「待部署」，实际代码已合 main 并 push 双远端、测试环境已部署 | `docs/design/plans.md`、`agent-memory.md` |
| 7 | `people.rs` 零测试 —— 404 / 空 body / 空 emp_no / 超时四条路径全无覆盖，而它是 SSO 接入的唯一改动点 | `crates/veda-server/src/people.rs` |
| 8 | `mysql/memory.rs`（577 行）零 `#[cfg(test)]` | — |

---

## 7. 不确定 / 未核实

- #3906 被 88 秒 revert 的**真实触发原因**（CI 红 / 误合 / review 卡）—— revert commit body 只有自动生成的一行，仓库内无 issue 文本。只能从重来版 #4126 推断设计上砍了什么。
- `context_assembler` 那批硬编码 quota / tier / penalty 数值的**调参依据** —— 无公开 eval 数据，且整包只有 14 天历史、上线当天就吃了 5 条 post-merge 修复。
- `train/` 框架的**实际收益** —— README 的 tau2 数字（retail 70.94→77.81）归属 0.3.22 的旧注入式 harness，不是 `session/train`。
- hermes 集成的代码不在该仓库（Nous Research 侧），官方 capability 文档对它的描述无法在仓内核验。

---

## 8. 建议的下一步

不预设排期，按 ROI 给顺序：

1. **T1 `detail_level` 解耦** —— 修已知线上 bug，且解掉 FDC 那条挂了 8 天的诊断
2. **T3 stats block** —— 最便宜的可观测性，为后续所有检索调优提供依据
3. **T4/T5 清掉两处纯负债** —— `last_used_at` 二选一、`topic` 连同继承逻辑删掉
4. **6.3 的 8 条现存缺陷** —— 半天的量，其中 #1 #2 #3 是外部可见的契约不一致
5. **T2 token 预算** —— 需要一次设计讨论（预算怎么在记忆/文件/QA 三类证据间分配）
6. **T6 过期清理 + memory reconcile** —— 等数据量起来再说，但 `handle_memory_sync` 的单测应该先补

明确**不做**：query rewrite、intent analysis、trajectory 事件流、hotness 排序、图/link、自动抽取的 LLM 增删改。这批在 `agent-memory.md §17`「明确不做」里的判断，这轮对标全部得到加强而非削弱。
