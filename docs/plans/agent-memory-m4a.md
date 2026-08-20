# Agent/团队记忆 M4a — 记忆浏览页最小版

> 施工图。架构定稿 [`../design/agent-memory.md`](../design/agent-memory.md) §15/§16；
> 上一篇 [`agent-memory-m3a.md`](../archive/plans/agent-memory-m3a.md)（操作者身份 +
> 部门域 + hybrid，已上线测试环境并归档）。**执行顺序在 M3b（qa_log 摄入）之前**，偏离 design §16 的
> M3→M4 顺序，理由见 §0。

## 0. 背景与目标

- M1/M2a/M3a 之后，记忆能存、能检索、能进 answer 出处，但**人没有任何入口看到它**——
  轻治理的前提「人人看得见才人人能改」（design §15）目前不成立。M3a 部署后
  dept/mine 域开始积累，一个看不见的 wiki 无法治理。
- **顺序对调（偏离 design §16，2026-08-20 Joe 拍板）**：浏览页先于自动摄入。
  三条理由：① 提名类功能（M3c）的落点是浏览页待办区，没有 surface 的提名是死信；
  ② M3b 自动摄入是第一条自动写路径，抽取质量要人肉盯，浏览页是它的质检面；
  ③ 本计划零 LLM、纯 CRUD + 前端，是剩余工作里最便宜、风险最低的。
- M4a = design §16 M4 减去两块：**待办区**（随 M3c 提名一起做）、**digest 渲染**
  （digest 触发条件未到，页面直接 live SQL 渲染；行格式提前对齐 digest 拼装格式
  `[mem:id] (日期) 内容 — 署名`，将来 digest 落地时 UI 只换数据源，§10.4 的
  「双消费者锁」照样成立）。

## 1. 范围（四项）

1. **store 枚举原语**：`MemoryStore` 加两个方法，与 `get_memories_by_ids` 同属
   「带 scope 过滤的读原语」家族——枚举同样强制 scope 条件，不开旁路：
   - `list_memories(filter, topic: Option<&str>, kind: Option<MemoryKind>, page, size)
     -> (Vec<Memory>, i64)`：ORDER BY updated_at DESC，SQL 侧过滤过期行（NOW()，
     同 context），LIMIT/OFFSET + COUNT 总数（记忆是千行量级，COUNT 不是问题）；
     size clamp 1..100（qa_log list 同款）。
   - `topic_counts(filter) -> Vec<(Option<String>, i64)>`：GROUP BY topic，
     NULL 归一组（UI 显示「未分类」）。
2. **REST 两端点**（`routes/memory.rs`，AuthWorkspace fs-only + 可选
   `X-Veda-Operator`，read-only `wk_` 可读）：
   - `GET /v1/memory/list?tab=team|dept|mine&topic=&kind=&page=&size=`
     → 新 DTO `MemoryPageResponse { items: Vec<MemoryItem>, total, page, size }`
     （`MemoryListResponse` 归 search/context 专用，不动）。
   - `GET /v1/memory/topics?tab=` → `[{ topic, count }]`。
   - tab 语义 = **单域查询**（见 §2）：team=(workspace,W)；dept=(dept,操作者部门)；
     mine=(principal,操作者) 且 origin ∈ {W, 空}（context mine 臂同款 origin 规则）。
     dept/mine 无操作者头 → InvalidInput 明确报错，不静默空；`team_only` 降级
     actor（M3a §1.2）同样只放行 team。service 层加 `list`/`topics` 薄方法收口
     tab→filter 的解析，REST 与将来的消费者共用。
3. **console 记忆页**：`#/console/memory/{workspace_id}`，`renderWsList` 行内
   「文件」旁加「记忆」入口（仅 fs kind）。鉴权复用 `getFsKey`/`setFsKey` 的
   per-workspace wk\_（同一把 key，fs 页存过就直接可用）。页面：
   - 顶部**身份栏**：可选输入 `wecom:xxx` / `emp:xxx`，存 per-tab（wk\_ 同模式），
     有值则所有请求带 `X-Veda-Operator`；未填时 部门/我的 页签置灰并提示。
   - 页签 = 团队 / 部门 / 我的；左侧 topic 目录（计数，点击过滤），右侧列表——
     行渲染 `[mem:{id}] ({日期}) {content} — {署名}` + [改][删]；「我的」页签内
     按 origin 分两组：随身（origin 空）/ 本项目。
   - 行内编辑（content/topic/expires_at → PATCH）、删除（confirm → DELETE）、
     「添一条」（POST，scope 按当前页签；返回的 neighbors 非空时展示
     「相似已有记忆」提示，引导改旧条——save 语义现成）。
   - 搜索框走既有 `GET /v1/memory/search`（scope=当前页签）。
   - i18n：main.ts `L` 词表补 zh/en 词条。
4. **admin 清理视角**（design §13 兜底「admin 可清」）：
   - `GET /admin/v1/memories?workspace=&kind=&page=`（**仅团队域**，按 last_used_at
     或 updated_at 排序可切）+ `DELETE /admin/v1/memories/{id}?workspace=`。
     既有 admin_token 门控。实现复用同一批 store/service 原语，显式传
     `(workspace, W)` 域——admin 不是旁路查询，只是把域参数从 key 语义换成显式参数；
     删除走 service 删除路径（向量清理 + 失败入 MemorySync outbox 同款）。
   - `admin.ts` 加记忆板块：workspace 选择 → 团队记忆列表（kind 筛选）+ 删除。

## 2. 设计决定

- **枚举归 MySQL**（M3a 拍板延续）：浏览/治理是关系型查询，Milvus 不掺和；
  MySQL 复核纪律天然覆盖（列的就是权威行）。
- **页签 = 单域查询，不是 context 三域合并**：浏览是治理视角，域边界要清晰可见；
  合并是检索视角的事。GateMem 口径顺延：list 的 scope guard 与读原语同一根列。
- **mine 页签范围 = origin ∈ {当前 W, 空}**：与检索所见一致（所见即所得）。
  跨项目的全量个人视图不做——那需要脱离 workspace 的个人面，等真实需求。
- **操作者身份 console 手填**：与 `X-Veda-Operator` 同一信任模型（M3a 拍板
  公司内互信、不加信任闸），不做登录系统。填错身份 = 看到错的「我的」域，
  自己的噪音自己纠正，与设计威胁模型一致。
- **动态区砍出 v0**：列表本身按 updated_at DESC 即最近变更 feed，独立动态区
  等有待办区（M3c）一起排版。
- **admin 面只进团队域**：清理场景 = 团队域投毒/错误事实；个人域不进 admin
  列表（本人可见原则，真有清理需求再议）。
- **平台侧（AI Workbench）不在本计划**：veda console 先行 dogfood；平台要嵌
  再走 project_data 包装一层（升级路径，零 veda 侧新逻辑）。

## 3. 测试（真实 MySQL/Milvus，`--ignored`）

- store 集成：list 分页 + topic/kind 过滤 + 过期行不出现 + total 口径；
  topic_counts 含 NULL 组。
- GateMem 口径延伸（memory_http_test.rs mega 扩）：B 身份 list A 的 mine 页签
  = 空且无存在性泄露；删除后 list 立即消失（硬删同款断言）。
- http e2e：三页签语义（dept 记忆只对同部门操作者可见）、无操作者 dept/mine
  → InvalidInput、read-only wk\_ 可 list 不可写（既有断言扩）、admin 枚举 +
  删除 + Milvus 向量随删消失（复用既有删除链断言）。
- console 手测：`docs/testing/manual-test-sop.md` 补记忆页一节
  （三页签看/改/删/搜/添 + 身份栏切换）。

## 4. 文档

ARCHITECTURE.md（web console 行 + memory 节边界更新）/ CHANGELOG.md /
`docs/api/db-workspace-api.md` + web zh reference（list/topics 端点、
MemoryPageResponse）/ design doc 状态行。

## 5. 不做（防散）

待办区与提名渲染（M3c）；digest 编译与渲染（触发条件不变：某域原子过几百条、
开场检索出噪音）；CLI memory 子命令（人用浏览页、agent 用 MCP，有真实要求再说）；
平台 Workbench 页面；编辑历史/回收站（design §17 已拒）；mine 跨项目全量视图；
时间范围等复杂筛选；动态 feed 独立区；个人域 admin 视角；list 的 Milvus 参与。

## 6. DoD

全部 e2e 绿 + 测试环境部署（server .161/.89）+ 真实冒烟：console 打开记忆页,
三页签看/改/删/搜/添一遍,身份栏填 `wecom:` 身份后「我的」页签可用,admin 面板
清掉一条团队记忆且检索立即消失;归档本 plan 并更新索引。
