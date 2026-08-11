# 未来演进方向（方向池）

> 外部对标产生的候选方向。**全部未排期、未实现**——只记结论和最小可行形态,
> 决定做哪条时再开正式 plan 进 `docs/plans/`。
> 后续新的对标调研直接在本文件追加小节。

---

## 对标 #1：TigerFS（2026-07-06 调研）

**项目**：[timescale/tigerfs](https://github.com/timescale/tigerfs)（TigerData/原 Timescale,Go + MIT,v0.7.0）。
把 PostgreSQL 挂载成文件系统（FUSE/NFS）,每个文件 = 一个 PG row,全 ACID。
参考：[官方文档](https://tigerfs.io/docs/)、[InfoQ 报道](https://www.infoq.com/news/2026/04/tigerfs-postgresql-filesystem/)、[HN 讨论](https://news.ycombinator.com/item?id=47430604)。

**它的核心竞争力**（按重要度）：

1. **可逆性即安全网**：每次 create/edit/rename/delete 记 operation log（UUIDv7 +
   actor + before/after 快照,历史存 hypertable）；`.savepoint/` 打书签、`.undo/`
   多粒度回滚（按 savepoint / 单条 log / 按用户）,undo 本身可再 undo。
   多 agent + 人并发写不做 git 式 branch/merge,而是「立即可见 + 原子回滚 +
   per-user 审计」。README 标题 "Every write is logged and undoable" 就是全部重心。
2. **文件系统即 API 的彻底性**：查询、历史、undo、schema 管理全塞进路径命名空间
   （`.by/.filter/.order/.export` 路径段编译成单条 SQL）,零 SDK 零客户端。
3. **事务原语当协调机制**：原子 `mv` 做 todo/doing/done 任务队列,多 agent 无锁协调。
4. **skills 分发策略**：自动安装 SKILL.md 到 Claude Code / Cursor / Codex / Gemini CLI
   等本机 agent 的 skills 目录,教安全模式（先 `.info/count` 再 ls 大表、动手前打
   savepoint、用管道查询省 token）——产品自带"教 agent 用我"的说明书。

**它的空白**：完全没有 embedding / 语义检索 / 摘要,检索能力只有 grep 和 SQL；
性能特征不明（大表、write-heavy 存疑）；data-first 写入无 undo。

**与 veda 的关系**：干净互补——tigerfs 有版本/审计/undo 但零语义层,veda 有
语义检索/三层摘要但零版本历史。veda 补上版本层后能力面是它的严格超集,
对内推广叙事可直接这样讲。

---

## 对标 #2：Agent / 团队记忆赛道（2026-08-07 调研）

**看了七家**：mem0、Letta（原 MemGPT）、腾讯 TencentDB-Agent-Memory、Zep/Graphiti、
Memobase、LangMem、basic-memory（含代码级核实）。

**跨项目共同结论**：写路径在退火不在加码（mem0 亲手砍掉自己发明的写时 LLM 决策）；
图数据库在退场（mem0 开源版删光图存储、腾讯只用 SQLite、Zep 的时序创新用几根列可近似）；
分层检索是共识（与 veda 文档三层同构）；巩固都是后台低频任务（= outbox worker + 防抖）。

**两块行业空白 = veda 的机会**（08-11 二轮核查 11 家后收窄措辞）：
① 共享域的**可见性控制**已有多家在做（腾讯 v2.0 四档 ACL、cognee dataset ACL），
但**治理闭环**——删除全链路传播、可测的遗忘——没有一家做完整；
GateMem 基准实测现有系统越权泄露 8.9%–33.9%、删除后仍答出 2.3%–37.2%，
veda 的分域 + 硬删目标是这两项确定性归零（集成测试可断言）；
② 「digest 只挑选拼装、不二次总结 + 引用可机械校验」11 家复核仍无一家实现。
veda 另有一个别人给不了的结构优势：**记忆和它的证据在同一个库**，`/v1/answer` 天然双源。

**完整方案与调研细节**：[`agent-memory.md`](agent-memory.md)（提案态，未排期；
08-11 已落三项拍板：可编辑零状态机 / 无审批 wiki 治理 / principal 归属标识）。

---

## 候选方向

### D3 Agent / 团队记忆

veda 除了存文档，再存一类「一句话一条的事实」，并区分个人域与团队域。
最小形态：两张表（记忆 + digest）+ principals 归属表 + 5 个 MCP 工具
（save/update/delete/search/context），记忆可编辑可硬删、零状态机，
团队域 wiki 式全员可写（无审批），检索合并两域。详见 [`agent-memory.md`](agent-memory.md)。
与 D1 共享 actor / 审计基建。

### D1 Operation log + per-file 版本历史/恢复（含 actor 审计）★ 最优先

**动机**：veda 目标场景（公司级多人/多 agent 共享 workspace）写入是纯覆盖,
没历史、没回滚、没"谁改的"。tigerfs 论证了这在 agent 场景是刚需（agent 会犯错,
人要能兜底）；也是 drive9 对标里 "revision-gated 异步回写" 的前置基建。

**veda 现有基础使成本可控**：

- 所有写走 `FsService` 单点,插桩一处
- 已有 `veda_fs_events` 表（SSE 在用）,事件流骨架现成
- content-addressed dedup（SHA256）→ 留旧版本**几乎零边际存储成本**
  （同内容不重复存,只需版本记录引用旧 hash、GC 时感知引用）

**最小可行形态**：

- 新表 `veda_file_versions (file_id, version, content_hash, op, actor, created_at)`,
  写路径同事务追加
- actor 来源：平台面 `GatewayUser`（creator/creator_name 已有）/ 数据面 `wk_` key
  label / 控制面 `vk_`
- API：`GET /v1/history/{path}`（版本列表 + actor）、`POST /v1/restore`
  （旧 hash 写回,产生新版本,不是时间倒流）
- 保留策略从简：保 N 版或 N 天,超期修剪版本记录；若未来做 blob/chunk GC,
  需把版本引用算进存活判定

**明确不做**（tigerfs 的 gold-plating,v0 不需要）：savepoint 体系、auto-savepoint、
undo-of-undo、跨文件事务组、多粒度按用户批量回滚。

### D2 `veda skill install`——本机 agent skills 分发

**动机**：tigerfs 用一条命令把 SKILL.md 装进各家 agent 的 skills 目录,
每台机器上所有 agent 自动学会安全用法。成本极低,是纯增益的 go-to-market。

**形态**：CLI 子命令 `veda skill install`,探测 `~/.claude/skills/` /
Codex / Cursor 等目录并写入 SKILL.md。内容教 veda 的省 token 安全模式：
`abstract` → `overview` → `cat` 分层读、search 先于全量 ls、grep（字面量）vs
search（语义）选择、collection / sql 用法。

**与现有工作的关系**：`docs/plans/onepaas-veda-skill.md` 是平台沙箱侧分发,
本条是本机 agent 侧分发,内容可共享一套模板,互补不冲突。

### 边缘（记录在案,不主动做）

- **frontmatter 虚拟列**：markdown YAML frontmatter 自动可 SQL 查。`veda_fs()`
  已解析 CSV/JSONL,补 frontmatter 属 nice-to-have,等真实需求
- **凭证不落盘**：tigerfs 调 tiger CLI 现取凭证,不写配置文件。对应 veda
  `config.toml` 明文 key 问题,已有 A-1 review 条目跟踪,内网场景不紧迫

### 明确不抄

- **路径即查询管道**（`.by/.filter/.order/`）：tigerfs 没有别的查询面才需要它；
  veda 有 `/v1/sql` + search API + CLI,塞进 FUSE 路径只加复杂度
- **data-first 模式**（挂任意 MySQL 表当文件浏览）：veda 不是数据库探索工具
- **NFS 协议支持**：macFUSE 已够,不加运维面

---

## 已有对标存档

- **drive9 / db9**（mem9-ai,2026-06 调研）：AI agent 工作区内核（LayerFS /
  semantic_tasks / quota / GET_LOCK 选主）+ 托管向量库。当时结论：最该借鉴
  持久化 semantic_tasks 与 revision-gated 异步回写——后者与本文件 D1 同源。
