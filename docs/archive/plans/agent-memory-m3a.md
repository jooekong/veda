# Agent/团队记忆 M3a — 操作者身份 + 工号/部门域 + 检索升级

> 施工图。架构定稿 [`../design/agent-memory.md`](../design/agent-memory.md) §5/§13/§14/§16；
> M2a 已归档 [`../archive/plans/agent-memory-m2a.md`](../archive/plans/agent-memory-m2a.md)。
> qa_log 自动摄入留 M3b。

## 0. 背景与目标

- M1 三节点全量、M2a 测试环境已上线。当前身份短板：wk_ key 即 principal，
  同一人从不同入口进来 = 多个个人域（design §14 标注的遗留问题）。
- 新需求（Joe 2026-08-18）：按人记忆（**工号唯一**，姓名/部门来自公司人员接口）、
  **部门维度记忆**、人↔部门可关联。
- 一并落地检索升级：memory 检索从纯 dense 升为 Milvus hybrid（dense+BM25），
  整体复用 fs chunk collection schema v2 的现成模式（`milvus.rs` jieba 分词 +
  BM25 function + RRF，生产 Milvus 2.6.14 已验证）。

## 1. 范围（六项）

1. **身份表拆分**：`veda_principals` 变纯「人/agent」表——
   `id, kind, emp_no(UNIQUE, NULL=未知/agent), display_name, dept_id, dept_name,
   profile_synced_at, created_at`（`source`/`external_id` 两列直接从表定义删除）；
   新表 `veda_principal_identities(source, external_id, principal_id, PK(source, external_id))`。
2. **人员目录接入**：`PersonDirectory` trait + HTTP 实现（config
   `[people] base_url / token / timeout`，默认 2s）。解析流：identities 命中且档案
   新鲜（<24h 常量）→ 直接返回；否则查目录拿 工号/姓名/部门 → 按 emp_no
   find-or-create principal → INSERT identity 指过去（**这一步就是跨入口合并**；
   合并只有「emp_no 原地补全 / identity 指向既有人」两种形态，**不存在 principal
   重定向**——见下条降级规则，identity-only principal 根本不会被创建）。
   **降级规则**（Codex R4 采纳后收敛）：已知身份 + 目录不可用 → 用缓存档案继续
   （读路径 fail-open）；**全新身份 + 目录不可用 → 该请求按无操作者处理**——
   检索降级为团队域、写 mine/dept 明确报错（快速失败，不造半身份把个人域
   写进错误的 principal）。目录故障永不阻塞团队域读写。
   **目录未配置模式**（SSO 接入前的长期形态，Joe 2026-08-18）：`[people]` 缺席时
   身份 = (source, external_id) 本身，直接建 principal（emp_no NULL、无部门）——
   Mine 域按入口分立可用，dept 域不可用；SSO 接上后同一 identity 懒查补全 emp_no。
   **合并只有原地补全一种**：emp_no 已被别的 principal 占（同人两入口在目录可用前
   各自建了人）→ 保持分立 + warn，不重定向不搬记忆——孤儿路径物理不存在。
3. **操作者透传**：请求头 `X-Veda-Operator: <source>:<external_id>`
   （v0 source ∈ `wecom` | `emp`），wk_ 鉴权通过后即生效，格式非法 → 400。
   `MemoryActor` 扩展为 `{workspace_id, principal_id, dept_id: Option<String>}`——
   有操作者时 principal/dept 取操作者的，没有时维持 M1 语义（key 即 principal）。
   `MemoryScope::Mine` 与 `SelfScope` 自此分歧：Mine=操作者个人域，Self=key 域。
   tunnel：**私聊带 `wecom:<userid>`，群聊不带**（受众规则见 §2）；MCP/CLI 走
   同一头（MCP 客户端配 header；CLI 加 `VEDA_OPERATOR` env，代码先行不发版）。
4. **部门域**：`MemoryScopeType::Dept`，scope_id = 目录返回的 dept_id——MySQL/
   Milvus 的 scope 列都是 VarChar，**零 DDL**。API `MemoryScope` 加 `dept`（写入
   需操作者有部门，否则 InvalidInput）；dept 域同 team 无 origin；Context filter 加
   第三臂；`allowed_scopes` 含操作者 dept（部门成员可改可删，wiki 同款）；
   `MemoryPatch` 加 `scope` 字段（**升域 = 同行 UPDATE + MemorySync，move 不
   copy**；目标域同 hash 已存在 → 冲突报错，由调用方删源行）。**MemorySync 契约
   （Codex R6 采纳）**：纯 scope move 也必须入队且事件携带**更新后**的 scope，
   服务端同步按新 scope 重嵌入 upsert——否则 Milvus 残留旧标量，目标域永远召不回
   （现实现只在 content 变化时入队，扩展时必须改掉这个前提）。`MemoryCitationRef`
   加 `scope` 字段（旧消费者忽略未知字段，降级同 M2a）。
5. **检索升级（Milvus hybrid）**：memory collection 定义改为 v2——
   `id + scope 三标量 + content(enable_analyzer, jieba) + vector + sparse_vector +
   BM25 function`，检索改走 `hybrid_search`（dense COSINE + BM25，RRF k=60，
   复用 fs 实现）。**域过滤表达式（scope/context filter）始终下推到 hybrid 两个
   子请求**（现状延续，Codex R7 澄清）——§5「不下推」仅指 topic/kind/时间这类
   新增过滤维度，域过滤下推是隔离与召回的前提，不在其列。**放弃 index-only 的交代**：content 进 Milvus 只喂 BM25 分词，
   检索输出仍只有 id+score，正文一律 MySQL 复核后重读——「Milvus 故障只降召回、
   永不吐已删/跨域文本」性质不变；fs chunk 全文本就在 Milvus，无新增敏感面。
6. **answer 三域注入**：answer 请求带操作者时注入 团队+部门+个人（一条
   Context filter，共池 top-5 不变），无操作者时维持 M2a 现状（仅团队域）；复核层
   **跨域 content_hash 去重，宽域优先 team > dept > mine**；注入模板与 tunnel
   出处行带域标签（`记忆(团队/部门/个人)：`）。

## 2. 设计决定

- **无历史数据迁移**（Joe 拍板 2026-08-18）：存量记忆是 dogfood 级，不保留。
  表和 collection 直接按新定义建；已部署环境升级时 DROP 旧
  `veda_memories`/`veda_principals`/memory collection 后重启自建，零迁移代码；
  回滚同理反向 DROP 让旧 binary 自建（deploy-runbook 各记一句）。节点按 runbook
  顺序逐台重启，不存在新旧实例并发建 collection 的窗口。
- **受众规则 = 操作者出现面**：注入域必须 ⊆ 答案受众可见域。群聊答案全群可见，
  注入个人/部门域会泄私——规则收敛为「tunnel 群聊不带操作者头」，服务端零新参数，
  无操作者自然只注团队域。
- **操作者头不加信任闸**（Joe 拍板 2026-08-18）：公司内互信，持 wk_ 即可带头，
  兜底 = 署名可追 + 人人可改 + admin 可清（design §13 同款）；按 key 标记 /
  签名断言等闸留作预案不实现。
- **部门授权接受 ≤TTL 的滞后**（Codex R3 拒绝其 fail-closed 建议）：调岗/撤权
  后旧部门记忆最多再可见 24h（缓存 TTL 上界）。部门记忆是 wiki 级共享知识不是
  权限敏感数据，零日精度的撤权是企业 RBAC 思维，与 design §13「不做 RBAC」一致
  地拒绝；离职场景由 企微账号注销（tunnel 源头）+ key 吊销覆盖。
- **RRF 无参数**：延续「不对空数据调参」。BM25/dense 权重、last_used_at 沉底、
  域间乘子等 dogfood 分布出来再议。
- **枚举归 MySQL、检索归 Milvus**：带 query 的检索 Milvus hybrid 为主，无 query
  的枚举（浏览/治理）是关系型查询归 MySQL（Milvus query() 无标量 ORDER BY），
  MySQL 复核是两者共同出口。list 端点本身砍出 M3a（Codex 建议采纳）：当前无
  消费者，随 M4 浏览页一起做。
- **部门不上卷**：只挂目录返回的 dept_id，不沿组织树上卷检索
  （升级路径：principal 缓存祖先链，filter dept 臂改 `in` 列表）。
- **search 不加过滤维度**：topic/kind/时间过滤等真实消费者出现再加。
- **不建 veda_depts 名录表**：dept_name 缓存在 principal 行上够用。

## 3. 测试（真实 MySQL/Milvus/embedding/LLM，`--ignored`）

人员目录在测试内起 stub（唯一不受控的第三方外部服务），其余照约定真实依赖。

- 身份合并 e2e：同一 emp_no 经 `wecom:` 与 `emp:` 两个 source 解析 →
  同一 principal，个人域互通。
- 部门域 e2e（GateMem 口径扩展）：A 部门操作者种 dept 记忆 → 同 workspace 的
  B 部门操作者检索/answer 不可见；A 部门操作者在**另一个 workspace** 可见
  （部门记忆跨 workspace 跟人走）。
- hybrid 关键词 e2e：种含罕见精确 token 的记忆 → 用该 token 检索命中
  （BM25 兜底实证）。
- 升域收敛 e2e：纯 scope move（content 不变）→ 目标域立即可检索、源域不可见
  （Milvus 新标量实证，防 R6 残留）。
- 注入面结构断言：无操作者的 answer 不出现个人/部门 citation。
- 单测：resolve_scope 操作者分歧（Mine vs Self）、跨域去重宽域优先、
  目录降级各分支（已知身份用缓存 / 新身份读降级写报错）、operator 头解析
  （合法 / 非法→400 / 缺席）。

## 4. 文档

ARCHITECTURE.md / CHANGELOG.md / design doc 状态行 /
`docs/api/db-workspace-api.md` + web zh reference（operator 头、`scope=dept`、
list 端点）。aidoc 维持不提 memory（Joe 拍板）。

## 5. 不做（防散）

qa_log 自动摄入 + 收敛提名（M3b）；`GET /v1/memory/list`（随 M4 浏览页）；
digest/画像（触发条件未到）；组织树上卷；排序加权/rerank/域配额；search 过滤
维度；topic/kind/时间的 Milvus 标量下推（**域过滤下推不在此列**，见 §1.5）；
veda_depts 名录表；gateway（passport）操作者源——等平台侧真实调用方；
操作者信任闸（key 标记 / 签名断言——Joe 拍板公司内互信不加，预案留档）；
每条记忆单独授权（design §13 已明确拒绝）。

## 6. DoD

全部 e2e 绿 + 测试环境部署（server .161/.89 + tunnel .89）+ 真实冒烟：
私聊 answer 引用 个人/部门 记忆（citation 带 scope）、群聊同问只引团队域、
工号在两个入口解析为同一人；归档本 plan 并更新索引。

## 评审记录（2026-08-18，Codex adversarial review，7 findings）

- **R1 操作者头可伪装（critical）→ Joe 拍板拒绝（2026-08-18）**：公司内互信，
  不加信任闸不做过度防御；风险知悉，兜底 = 署名 + 人人可改 + admin 可清；
  key 标记 / 签名断言留预案不实现。
- **R2 关系型回滚断裂（critical）→ 已被拍板废弃**：Joe 定无历史数据迁移，
  回滚 = 反向 DROP 自建（§2 首条）；expand/contract 建议不适用。
- **R3 目录缓存滞后保留部门授权（high）→ 拒绝**：≤24h 滞后对 wiki 级共享知识
  可接受，fail-closed 会让目录故障砍掉部门域可用性，撤权精度是 RBAC 思维（§2）。
- **R4 identity-only principal 合并孤儿（high）→ 采纳，以删代修**：去掉半身份
  fallback，新身份 + 目录故障 = 按无操作者降级（§1.2），重定向/孤儿路径整体消失。
- **R5 启动 drop+重建并发窗口（high）→ 已被拍板废弃**：启动自迁移机制已删，
  升级是手工 DROP + runbook 顺序重启。
- **R6 scope move 的 MemorySync 契约（medium）→ 采纳**：纯 move 也入队、事件带
  新 scope、同步重嵌入（§1.4 + 升域 e2e）。
- **R7 标量下推歧义可致召回黑洞（medium）→ 采纳（文字澄清）**：域过滤恒下推
  hybrid 两个子请求，「不下推」仅指新增过滤维度（§1.5、§5）。
- 附带建议 list 端点 YAGNI → 采纳，移 M4（§5）。

## 执行记录（2026-08-20 归档）

- **实现**：`614a1c7`（feat(memory): M3a），两轮 Codex 评审 findings 全部裁决入档（下方两节）。
- **部署**：2026-08-19 `.89` build（sha `5350fb47…`，16:49），16:51 `.89` / 16:53 `.161`
  swap+restart，tunnel `.89` 同批重启；M3a 门禁（DROP principals/identities +
  TRUNCATE memories + drop collection + purge `memory_sync`）已在 veda_it 执行
  （runbook 撞号注记即当日实测）。**生产 `.85` 未部署**，随下次发版窗口（连同 M2a）。
- **冒烟**（2026-08-20，server API 层七项全过，测试环境真实依赖）：
  操作者头格式非法 → 400；dept 写入按 identity-only 正确拒绝（`[people]` 未配置）；
  `wecom:`/`emp:` 两入口分立（identity-only 预期）；mine 域 save/search 经 wecom
  操作者可用；answer 带操作者引用个人记忆（citation `scope:"mine"`）；同问无操作者
  **零个人域泄露**；团队记忆 citation `scope:"team"`。tunnel 真实流量：2026-08-20
  私聊三问 `answer ok`（chattype=single 带操作者头路径，零报错）；群聊未有真实流量——
  server 侧无操作者行为已冒烟，tunnel 侧不带头是 fail-closed 缺省（R4），风险最低方向。
- **DoD 偏差**：「私聊引用部门记忆」「工号两入口解析为同一人」两项**正向**验证被
  `[people]` 未配置阻塞（SSO 契约待接，identity-only 是拍板的长期形态）——e2e 已用
  stub 目录覆盖，真实环境正向验证顺延到 `[people]` 接入时补两条冒烟。

## 评审记录（2026-08-18 第二轮，Codex 原生 review 实现 diff，4 P1 + 4 P2 全采纳）

- **R1 self 与 mine 未分歧（P1）→ 修**：plan §1.3 写了、实现漏了。`MemoryActor` 加
  `self_principal_id`（操作者在场时=key principal），`scope=self` 落 agent 域；
  路由层顺手把 key 解析提到公共路径（本就要 ensure，无额外查询）。
- **R2 目录明确答「查无此人」仍沿用旧部门（P1）→ 修**：已知身份保个人域可用，
  但 dept 请求级剥离（不写库——行保持 stale，之后每次都重问目录）；部门授权
  滞后回到 ≤TTL 承诺内。
- **R3 降级回落 key 域暴露共享 key 私有行（P1）→ 修**：plan 写的是「检索降级为
  团队域」，实现却回落 key 语义。改为 `MemoryActor.team_only`：service 层统一
  收口——私域 scope 全拒、context 塌缩团队域、可写域只剩团队；REST/MCP 的
  reject_degraded_write 补丁随之删除（服务端单点执法）。
- **R4 chattype 缺失/未知按私聊处理（P1）→ 修**：fail-closed——只有显式
  `single` 且无 chatid 才带操作者头，其余一律按群可见处理。
- **R5 CORS 缺 x-veda-operator（P2）→ 修**：allow_headers 补一项。
- **R6 目录空 body/204 判成故障（P2）→ 修**：成功空响应=查无此人，不进降级路径。
- **R7 round-trip 未变的 scope 清掉 origin（P2）→ 修**：store 层比对当前域，
  scope 未变=非 move（不清 origin、不入 sync）。
- **R8 三域去重下 OVERFETCH=2 不够（P2）→ 修**：2→3。
