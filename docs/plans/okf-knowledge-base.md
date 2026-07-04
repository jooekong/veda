# Veda × OKF 知识库设计

> 状态：设计草案，待 Joe review（未开工，未改代码）
> 来源：2026-06-22 方向讨论 —— 分布式 = 多源联邦；库选型 = fs kind；隔离 = 默认 bundle 隔离，联邦为未来加法
> 参考：Google [Open Knowledge Format](https://cloud.google.com/blog/products/data-analytics/how-the-open-knowledge-format-can-improve-data-sharing/) 博客；veda 现状已逐条核实，全文代码论断带 `文件:行号`
> 一句话：把 veda 做成 **OKF 的高性能、可联邦检索 runtime** —— “git for knowledge, with search”

## 0. TL;DR

- **定位**：OKF 负责“知识长什么样、怎么交换”（可移植 markdown 格式），veda 负责“知识怎么存、怎么搜、怎么挂、怎么扩”（检索 runtime）。两者是同一件事互补的两半。
- **库选型**：`fs` kind。OKF bundle 是 markdown 文件 + 目录 + 链接，`db` kind 是裸向量，装不下文件/目录/FUSE/SQL。
- **差异化**：市面方案非此即彼 —— 要么有 runtime 没可移植格式（向量库 / catalog），要么有格式没 runtime（OKF 裸用）。veda+OKF 两头都占，独门武器是 **SQL × 语义检索融合 + L0/L1/L2 分层 + FUSE + 联邦**。
- **节奏**：P0 用 OKF 官方 sample bundle 做 go/no-go（按需检索 vs 全量读），数据好看再投 P1（摄入扎实 + 导出）→ P2（关系图）→ P3（联邦）。
- **契合度**：摄入自动化、混合检索、SQL、FUSE、分层摘要、多租户**全已有**；三个硬缺口待建 —— frontmatter 结构化、关系图、结构化过滤（服务层目前是死参数）。

---

## 1. 背景：两个半成品，拼成一个完整品

### OKF 是什么

Google 推的 **知识可移植格式**：markdown + YAML frontmatter（字段 `type`/`title`/`description`/`resource`/`tags`/`timestamp`，仅 `type` 必填）+ 目录结构 + 文件间 markdown 链接（构成关系图）。打成 tarball、塞进 git repo、挂到文件系统、人和 agent 共同编辑。

它的设计哲学是 **“format, not platform”**、**“never require a proprietary account or SDK to read, write, or serve”** —— 也就是**故意不提供** runtime、检索、向量、存储。

| OKF 有 | OKF 故意没有 |
|---|---|
| 可移植格式（markdown+yaml） | 检索 / 向量 / 语义搜索 |
| git 友好、人/agent 共编 | runtime / 服务 |
| 关系（markdown 链接图） | 存储引擎 |
| 厂商中立 | SQL / 挂载 / 多租户 |

### veda 缺什么 / 有什么

veda 恰好是 OKF 留空的那一半：有 runtime + 混合检索（语义/全文/hybrid）+ SQL（DataFusion）+ FUSE + L0/L1/L2 分层摘要 + 多租户。但 veda 的知识表示是**私有且封死的**（schema 是 MySQL 里的 JSON blob、collection 不可演进、不可被别的工具读），且数据表示不可移植。

**结论**：OKF 的洞 = veda 的强；veda 的洞 = OKF 的强。把 veda 做成 OKF 的检索 runtime，是两边都补齐的最短路径。几个契合点干净得不像巧合：

| OKF 说的 | veda 已经有的 | 证据 |
|---|---|---|
| “mountable on any filesystem” | FUSE 挂载 | `crates/veda-fuse/` |
| 痛点：models “search the same documents for the same facts over and over” | 按需混合检索 + L0/L1/L2 分层 | `routes/search.rs`、`types.rs:451-463` |
| bundle = tarball / git repo（纯文件） | fs 文件树，天然适合同步 | `routes/fs.rs` |
| 关系靠 markdown 链接构成 graph | 缺，但可补（着力点 C） | 见 §7 |

---

## 2. 差异化竞争：为什么是 veda + OKF

### 2.1 市面方案的二分

所有现有方案都落在一条线的两端，**没有一个同时占住“可移植格式”和“检索 runtime”**：

```
有 runtime / 没可移植格式            ←——————————→            有可移植格式 / 没 runtime
  向量库(Pinecone/Weaviate)                                      OKF 裸用 + agent
  catalog(DataHub/Atlan)                                         markdown wiki

                         ★ veda + OKF：两头都占 ★
```

### 2.2 veda + OKF 的五个独门支点

1. **SQL × 语义检索融合**：`SELECT s.path, s.score FROM search('query','hybrid',50) s JOIN files f ON s.path=f.path WHERE f.path LIKE '/metrics/%'`。OKF frontmatter（结构化）+ body（语义）天然契合 —— 结构化字段用 SQL 过滤，body 用语义检索，DataFusion 把两者 JOIN。**纯向量库只有 filter DSL，给不了完整 SQL JOIN。**（`engine.rs:73-129`、`search_table.rs:18-26`）
2. **L0/L1/L2 分层**：agent 先拿 L0 abstract（~100 token）判断相关性，需要再钻 L2 原文。**直接解 OKF 那句“别反复全量读”的痛点。**（`types.rs:451-463`）
3. **FUSE 挂载**：OKF 说“mountable on any filesystem”，veda 真有，agent 可以像读本地文件一样读知识库。
4. **格式可移植（OKF）**：知识不锁在 veda —— git 管理、人/agent 共编、随时 tar 走、被任何工具读。这是反 vendor lock-in 的根本卖点，公司级服务尤其需要。
5. **联邦（未来）**：多 bundle 统一检索，像 git 而非分库（见 §8）。

### 2.3 逐个对手

| 对手 | 它强在 | 它的洞 | veda+OKF 怎么赢 |
|---|---|---|---|
| 纯向量库（Pinecone/Weaviate/Qdrant） | 相似度检索快、规模化成熟 | 知识=黑盒向量碎片：无结构、无关系、不可读、不可移植；无 SQL JOIN；agent 反复重嵌 | 可读 markdown + frontmatter 结构化 + 关系图 + SQL×语义 + 可移植 |
| GraphRAG / Neo4j+LLM | 显式关系、可推理 | 重；图构建 + schema 维护贵；构建慢 | 轻量关系图（markdown 链接），够用不过度（见 §7） |
| Data catalog（DataHub/Atlan/Collibra） | 元数据治理、lineage | 锁私有 API；偏“给人看的治理”，不是“给 agent 的检索” | 开放格式（OKF 正冲它去）+ 给 agent 的混合检索 |
| OKF 裸用 + 自配 agent | 可移植、git 管理、厂商中立 | 无检索 runtime，agent 全量读，规模化贵且慢 | 加上检索 / SQL / FUSE / 分层 / 联邦 |
| RAG 框架（LlamaIndex/LangChain） | 灵活、生态大 | 是库不是服务，要自己拼存储+检索+运维+多租户 | 开箱服务 + 多租户 + 可观测 + 部署 |

### 2.4 “为什么不直接拿 OKF + 一个向量库拼？”

因为你得自己造：文件树、FUSE、SQL 引擎、分层摘要、多租户、可观测、部署 —— **那就是重新造一个 veda**。veda 把这些集成在一个服务里，且 fs 模型天然匹配 OKF 的“文件 + 目录”形态。集成度本身就是护城河。

> 补充判断：押注 OKF 这个新格式（Google 2026 才推）风险不大 —— 它就是 markdown+yaml，即使标准没普及，“可移植 markdown 知识库 + 检索”本身成立，不强依赖 OKF 胜出。而 veda 支持它几乎是“顺便”：fs 本来就存 markdown，OKF 只是加了一个 frontmatter 约定 + 链接约定。低成本下注。

---

## 3. 核心设计：OKF → veda 映射

| OKF 概念 | veda 落点 | 状态 | 证据 |
|---|---|---|---|
| bundle | workspace（`fs` kind），**1 bundle = 1 团队 / 1 数据域** | 已有 | — |
| document（`.md`） | file | 已有 | `routes/fs.rs:96-129` |
| 目录结构 | dentry 树（parent_path/path） | 已有 | `mysql.rs:385-399` |
| markdown body | file content → 自动 chunk/embed/L0L1 | 已有（异步） | §4 |
| frontmatter（type/tags/…） | **v0**：随 body 一起 chunk（可被语义命中）；**P1**：结构化落点（决策点，见 §13） | 需新建 | `types.rs:339-370`（file/dentry 无 metadata 列） |
| markdown 链接（关系） | **P2**：关系边存储 | 需新建 | grep 0 命中；只有 dentry 树 |

**bundle 粒度（设计决策）**：1 bundle = 1 个团队 / 1 个数据域。别细到 1 文档 = 1 bundle（workspace 和 `wk_` key 会爆量），也别粗到多团队塞一个 workspace（将来想拆隔离才是真痛点）。粒度比“要不要联邦”更难纠正，是单向门，值得现在拍。

---

## 4. 摄入流程（着力点 A）

```
git pull / tarball  ──►  解析 bundle  ──►  PUT /v1/fs/{path}（保持目录结构）
                                                │
                                                ▼（异步，自动）
                              outbox → worker → chunk → embedding → L0/L1 summary → Milvus
```

**核心利好：摄入链路是现成且自动的，importer 很薄。** PUT 一个 `.md` 文件后，切 chunk → embedding → L0/L1 summary 是**异步自动**发生的，不需要单独触发：

- 写文件与 outbox 入队**同事务原子**：`ChunkSync` + `SummarySync` 两条事件随写入入队（`crates/veda-core/src/service/fs.rs:373-387`）。
- 后台 worker 轮询处理：chunk→embed（`crates/veda-server/src/worker.rs:280-393`）、L0/L1 摘要（`worker.rs:454-521`）。worker 启动时自动 spawn（`crates/veda-server/src/main.rs:151-170`）。

**importer 要处理的注意点**（写在文档里防踩坑）：

1. **异步最终一致**：PUT 返回 200 时索引**还没建好**，延迟 = worker poll + embed/LLM 耗时。importer 不能假设 PUT 后立刻可检索；验证脚本要轮询或等待。
2. **无 `[llm]` 配置 → 无 L0/L1 摘要**，但 chunk embedding 不依赖 LLM，**语义/全文检索照常工作**（`worker.rs:202-207`）。分层（支点 2）需要配 LLM。
3. **增量同步**：靠 `git diff` 只 PUT 变化的文件；内容没变的重复 PUT 不会重复 embed（`last_embedded_content_hash` watermark，`worker.rs:306-311`）。删除要显式调 fs 删除端点。

---

## 5. 检索设计（着力点 A）

### 5.1 三模式 + 分层（解 OKF 全量读痛点）

`POST /v1/search`（`crates/veda-types/src/api.rs:154-162`）：`mode` ∈ hybrid/semantic/fulltext，`limit` ≤100，`path_prefix` 过滤，`detail_level` ∈ abstract/overview/full。返回 `SearchHit` 可挂 `l0_abstract`/`l1_overview`（`types.rs:498-527`）。

**用法**：agent 先 `detail_level=abstract` 拿 L0 摘要（便宜）判相关性，命中后再取 L2 原文 —— 这正是 OKF 痛点的解药。

### 5.2 SQL × 语义融合（独门武器）

DataFusion 引擎（`crates/veda-sql/src/engine.rs:60-154`，只读 SELECT）注册了 `files` 表 + `search()` UDTF + `embedding()` UDF：

```sql
-- 语义检索 + 路径/结构化过滤组合（现成能力）
SELECT s.path, s.score, s.content
FROM search('如何计算周活', 'hybrid', 50) s
JOIN files f ON s.path = f.path
WHERE f.path LIKE '/metrics/%'
ORDER BY s.score DESC;
```

`search()` 输出列 `file_id, chunk_index, content, score, score_type, path`（`crates/veda-sql/src/search_table.rs:18-26`）。这是纯向量库给不了的：**在一条 SQL 里把语义检索和结构化过滤 JOIN 起来。**

### 5.3 现状缺口（诚实标注）

- **结构化过滤在服务层是死参数**：`/v1/search` 除 `path_prefix` 外无 tags/type/metadata 过滤（`deny_unknown_fields` 直接拒）；`CollectionSearchRequest.filter` 解析了但**从不生效**（`crates/veda-server/src/routes/collection.rs:107-111`）；collection 底层 search 只按 `workspace_id` 过滤（`crates/veda-store/src/milvus.rs:1655-1677`）。
- **唯一现成的“语义 + 结构化”组合路径 = SQL 引擎**（§5.2）。
- `search()` UDTF 不暴露 `path_prefix`（内部写死 `None`，`search_table.rs:165`）—— 要按路径过滤得外层 `WHERE path LIKE` 后筛。
- **结论**：v0 检索靠 `/v1/search`（语义/hybrid）+ SQL（需要结构化过滤时）。frontmatter 精确过滤的服务层支持留到 P1。

---

## 6. 导出 / producer（着力点 D）

veda 把 workspace 内容 + worker 已生成的 L0/L1 摘要，导出成**合规 OKF bundle**（frontmatter + body + 链接）。

- 复用现成的 summary（`veda_summaries`），几乎是“顺便”。
- 价值：veda 既是 OKF 的 consumer 也是 producer（OKF 强调 producer/consumer 独立），知识可随时 tar 走 → **反 vendor lock-in**，公司级服务的开放性卖点。
- 边际成本低，建议和 P1 一起做，形成“读 OKF → 检索 → 写回 OKF”的闭环。

---

## 7. 关系图增强（着力点 C）

OKF 的 markdown 链接（`[orders](/tables/orders.md)`）构成跨文档关系图。veda **目前零基础**：除 dentry 目录树外，没有任何文件↔文件关系存储（grep `link/edge/relation/graph` 全仓 0 命中）。

### 7.1 存储方案（决策点）

| 方案 | 做法 | 取舍 |
|---|---|---|
| **(a) 专门 link 表（倾向）** | 新建 `veda_links(src_path, dst_path, link_type)`，摄入时解析 markdown 链接写入 | 轻、查询直接、无冗余 embedding；要加表 + 解析 |
| (b) structured collection 存边 | 复用 `veda_coll_<id>` 存边 | 复用现成；但**每行强制 embed**（`collection.rs:124-157`）→ 纯关系边产生无用向量，浪费 |

倾向 (a)：符合简洁原则，关系边是纯结构化数据，不该被迫塞向量。

### 7.2 graph-augmented retrieval

命中一个 doc 后，沿 link 表扩展邻居（“orders 表 JOIN 了哪些表”→ 把关联表一起召回）。比纯向量 RAG 多了结构，比重型 GraphRAG 轻。

**缺口**：DataFusion 无递归 CTE，多跳遍历 = 应用层 BFS（多次单跳查询）。v0 只做 1 跳，够用。

### 7.3 价值时机

关系图在**联邦下价值最大**（跨团队：team-a 的表链到 team-b 的表 = 联邦知识图）；单 bundle 时性价比一般。所以排在 P2，联邦（P3）前的铺垫。

---

## 8. 联邦演进（着力点 B，未来）

**默认 bundle 隔离，联邦是未来的纯加法。** 关键事实：fs 的 chunks/summaries 是**共享 collection**（`veda_chunks`/`veda_summaries` 常量，`crates/veda-store/src/milvus.rs:15-16`），所有 bundle 物理上在同一 collection，靠 `workspace_id` **标量字段**隔离（不是物理分区）。所以：

- bundle 隔离 = 检索 `workspace_id == 当前 ws`
- 跨 bundle 联邦 = 检索 `workspace_id IN (a, b, c)`

**联邦在数据层几乎免费** —— 不重建布局，只放宽一个谓词。要新建的只有上层：联邦查询入口 API + “一个调用方能跨哪些 bundle 查”的鉴权（可复用控制面已有的 scoped key `allowed_workspaces` 思路）。

**单向门**：隔离→联邦是放宽（易）；不隔离→隔离是拆数据（难）。默认隔离站在可逆一侧。配合“无需向后兼容”，真要联邦那天数据还在 OKF bundle（git）里，大不了重新 ingest。**所以现在为联邦预埋任何抽象都是负收益。**

> 注意：fs 共享 collection ⇒ 联邦再多 bundle 也只有两个 collection，**不撞** db kind 那个“每 ws 一 collection、~1500 撞 Milvus 内存”天花板（那是 `db` kind 的事，`milvus.rs:34`）。fs 路径无此问题。

---

## 9. 核心假设与 go/no-go 验证

整个方向押在一个假设上：

> **agent 用 veda 按需检索 OKF bundle，显著优于把整个 bundle 全量塞进 context。**

不成立，veda 就只是“又一个能读 OKF 的工具”（OKF 故意人人可读），整个方向没意义。所以先用最小代价验证：

1. 拿 **OKF 官方现成 sample bundle**（GA4 / StackOverflow / Bitcoin，博客直接给），不自己造数据。
2. 写**最小 importer**：把每个 `.md`（含 frontmatter）原样 PUT 进一个 fs workspace，保持目录结构。复用现成自动索引。基本不写新代码。
3. 对比实验：同一批问题，**agent 按需检索（`/v1/search` + L0 分层）vs 全量读**，比 **token 成本 / 延迟 / 答对率**。

**通过标准（待 Joe 定阈值）**：按需检索在 token 成本显著下降的同时，答对率不低于全量读（或差距可接受）。通过 → 投 P1；不通过 → 止损，省下 P1-P3。

---

## 10. 风险与取舍

| 风险 / 约束 | 影响 | 对策 |
|---|---|---|
| 文件存 MySQL，单文件 ≤50MB | OKF doc 都是小 markdown，**基本不受影响**（利好） | 无需处理；大附件本就不属 OKF |
| 摄入异步最终一致 | PUT 后非立即可检索 | importer/验证脚本轮询等待（§4） |
| frontmatter 无处放 | 结构化过滤 v0 缺 | v0 随 body 语义命中；P1 决定落点（§13） |
| 关系图零基础 | 着力点 C 成本最高 | 排 P2；v0/P1 不依赖 |
| 结构化过滤服务层死参数 | 精确过滤受限 | v0 走 SQL；P1 补 search filter |
| 无递归 CTE | 多跳图遍历受限 | v0 只 1 跳；应用层 BFS |
| OKF 标准成熟度未知 | 押注新格式 | 成本极低，不强依赖标准胜出（§2.4） |

---

## 11. 路线图

| 阶段 | 范围 | DoD / Gate |
|---|---|---|
| **P0** go/no-go | 最小 importer + 官方 sample bundle + 检索 vs 全量读对比 | 假设验证通过（§9）才进 P1 |
| **P1** 摄入扎实 + 导出（A+D） | importer 健壮化（增量同步/删除/错误）；frontmatter 结构化落点；OKF 导出闭环 | 真实 bundle 跑通；frontmatter 可过滤 |
| **P2** 关系图（C） | link 表 + 摄入解析链接 + 1 跳 graph-augmented retrieval | 关系召回质量可量化提升 |
| **P3** 联邦（B） | `workspace_id IN(...)` + 跨 bundle 鉴权 + 联邦查询 API | 多 bundle 统一检索可用 |

每阶段独立产出价值，且都有 gate，可随时止损。

---

## 12. 明确不做（非目标）

- **不做数据分片 / 复制 / 共识**：联邦 ≠ 分库（§8）。
- **不做重型知识图谱 / Neo4j**：关系图保持轻量（markdown 链接级，§7）。
- **不做 frontmatter 复杂 schema 演进**：OKF frontmatter 半固定，按需加字段即可。
- **v0 不做结构化过滤**：靠语义 / SQL 兜底（§5.3）。
- **不做向后兼容**：可自由打破旧格式（Joe 原则）。

---

## 13. 待 Joe 拍板的决策点

1. **bundle 粒度**：采用“1 bundle = 1 团队 / 1 数据域”？（§3，单向门，建议现在定）
2. **frontmatter 落点（P1）**：倾向 **(A) 给 `veda_files` 加 metadata JSON 列**（最自然、避免文件树与 collection 两套同步、无强制 embed 浪费），还是 (B) structured collection 旁路（复用现成但有冗余）？
3. **关系存储（P2）**：倾向 **(a) 专门 `veda_links` 表**（轻、无 embedding 冗余），还是 (b) collection 存边？
4. **v0 范围**：是否就锁定为“裸 PUT + 现成检索 + 对比实验”，不碰 frontmatter/关系？
5. **调用方接口**：agent 直接写 SQL（灵活、现成），还是先封装一个高层“OKF 检索 API”（易用、但要新建）？
6. **go/no-go 阈值**：token 成本下降多少、答对率容忍多大差距算“通过”？（§9）
