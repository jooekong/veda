# wiki 知识库 × Coding Agent 接入方案（MCP + 入库体验）

> 状态：**已过三轮 Joe review（2026-07-22），P0 开工**——①Word 已随 0.1.20 上线出方案；HTML 提取、`veda sync` 降级 P2（理由见各节状态框）；②MCP 形态定为 **veda-server 原生 `/mcp` 端点**（用户零安装，纯 json 配置）；③**stdio 形态被否决**（Joe 拍板：veda 只支持 Streamable HTTP，不做 stdio）
> 来源：2026-07-22 需求 —— 公司同事想把 wiki 文件传进 veda fs 知识库，在 Coding Agent（Claude Code / Cursor / Codex）里让 AI 检索知识库获取更准确的上下文
> 现状论断均经代码核实（2026-07-22），关键论断带 `文件:行号`
> 关联：[`okf-knowledge-base.md`](okf-knowledge-base.md)（知识**格式/生态**战略，未开工）与本方案互补不冲突——本方案解决眼前的**接入与体验**工程，OKF 是格式层演进；两者共享「L0/L1/L2 分层喂 agent」的核心思路

## 0. TL;DR

- **这个场景基本是 veda fs 的主场**：hybrid 检索（dense+BM25 RRF+jieba）、L0/L1/L2 分层、`/v1/answer` agentic RAG、Word/PDF 提取（0.1.20）全部已上生产；今天就有两条能走通的路（CLI+skill.md / 裸 REST）。
- **「最好的接入体验」= MCP**。理由三条：skill 机制是 Claude Code 私有（Cursor/Codex 各玩各的，MCP 是唯一全家通吃的协议）；schema 化 tool call 比「读文档拼 bash」可靠；配置可进 git 仓库团队分发。**形态（07-22 二轮 review 定）：veda-server 原生 `/mcp` 端点（Streamable HTTP，stateless）**——用户侧纯 `.mcp.json` 配 `url`+`Authorization: Bearer wk_`，**零安装零 init**；工具直调 service 层，鉴权复用 `AuthWorkspace`。stdio 子命令形态降 P2 备选。全仓目前零 MCP 代码。
- **入库侧结论（07-22 review 定）**：Word 已上线；HTML 本就是 UTF-8 文本、今天就能入库能搜，「标签噪声拉低质量」是未实测的推断 → **降 P2，先实验后开发**；增量上传 `cp -r` 重跑已由服务端 sha 短路解决，删除同步有 FUSE+`rsync --delete` 零开发替代 → **`veda sync` 降 P2**。
- **分期**：P0（server `/mcp` 端点）→ P1（env 鉴权 + 索引可见性 + `veda ask`）→ P2 背包看反馈/实测。

## 1. 场景与成功标准

两个角色：

- **A = wiki 管理员**：把公司 wiki（一批文件）传进一个 fs workspace，之后持续维护（增改删）。
- **B = 用 Coding Agent 的开发**：在 Claude Code/Cursor 里干活，期望 agent 需要业务/架构背景时**自动**去查知识库，拿回带出处的上下文，而不是自己开浏览器翻 wiki。

**成功标准（本方案的北极星）**：

1. A 的维护成本：改了 10 页 wiki 后，一条命令、只传变化的部分、能确认「什么时候可搜」。
2. B 的接入成本：≤5 分钟完成配置（拿到 wk_ 之后），之后**零学习成本**——agent 自己会用工具，B 不需要教。
3. 检索质量：中文 wiki 页（含 Word/HTML 来源）能被语义+关键词命中，出处可回溯到具体文件。

## 2. 现状盘点（已核实）

### 2.1 今天就能走通的

| 能力 | 现状 | 证据 |
|---|---|---|
| 批量上传 | `veda cp -r` 递归、跳 symlink、内置 ignore（.git/node_modules/…）、单文件失败跳过（连续 10 次才中止） | `veda-cli/src/main.rs:1761-1855` |
| 重跑幂等 | 客户端带 `If-None-Match: "<sha256>"`，服务端 content_unchanged 短路，不重复 embed | `veda-cli/src/client.rs:185-194` |
| 格式：Markdown | 一等公民——chunking 按 `#`..`######` 标题切段 + CJK 按 1 token/字换算防超限 | `veda-pipeline/src/chunking.rs:32-75,24-30` |
| 格式：PDF | `pdf-extract` 抽文本层进 Milvus，原件可下载（无 OCR，扫描件搜不到） | `veda-pipeline/src/extraction.rs:29-32` |
| 格式：Word | **已上线（0.1.20，2026-07-22 三节点 + 存量 backfill）**：.docx（zip+quick-xml）/.doc（自写宽松 CFB），`SourceType::Word` → ExtractSync，提取文本存 `veda_file_extracts`（sha 防陈旧），cat/preview 返提取文本 | `veda-pipeline/src/word.rs`；SOP `docs/word-e2e-sop.md` |
| 格式：HTML | **能入库能搜**（合法 UTF-8 走文本路径，正文词 BM25/语义均可命中），只是标签一并入库——是质量优化空间，不是功能缺口（07-22 review 定性） | `routes/fs.rs` UTF-8 sniff 分流 |
| 检索 | `/v1/search`：hybrid=dense+BM25 RRF（Milvus 2.5 原生）、jieba 中文、`path_prefix` 子树过滤、`detail_level` 三层、`deny_unknown_fields` 参数错直接 400 | `veda-server/src/routes/search.rs`、`veda-types/src/api.rs:161-169` |
| 分层省 token | L0 abstract（~100 tok）/ L1 overview（~2k tok）/ L2 原文，`GET /v1/abstract|overview/{path}` | `ARCHITECTURE.md` 三层信息模型 |
| RAG 问答 | `/v1/answer(/stream)`：LLM 自主多轮 search/read_file，答案带可验证 `[n]` 引用；已上生产（企微 bot 在用） | `veda-server/src/routes/answer.rs` |
| agent 抓手 | `skill.md`（343 行）已发布，install.sh 自动装 `~/.claude/skills/veda/SKILL.md`；CLI 全局 `--json`（ls/search/grep/collection search/sql，JSONL）、stdout/stderr 分离、`--help` 自描述强 | `web/public/docs/zh/skill.md` |

### 2.2 缺口清单

| # | 缺口 | 影响 | 证据 |
|---|---|---|---|
| G1 | **无 MCP server**（全仓零代码，仅计划提及） | B 的接入只剩「装 CLI+init」或「手搓 REST」，每个同事都要教一遍 | grep 全仓；`onepaas-veda-skill.md:12,135` |
| G2 | HTML 标签随正文入库（无提取分支）——**对检索质量的实际影响未实测**，语义向量一路受影响概率大于 BM25（标签为英文 token，不与中文查询相撞）；agent read_file 读到标签汤费 context | 质量优化项而非功能缺口；待真实语料实测（§6） | `extraction.rs:8-21` 无 html 分支 |
| G3 | 删除不同步——`cp -r` 只增改不删，本地删页远端仍被检索命中（过期信息污染）。增量上传**已被服务端解决**：`If-None-Match` sha 短路，重跑 `cp -r` 不重复 embed（带宽照花，内网可接受） | 有零开发替代：FUSE 挂载 + `rsync --delete`（§8） | `client.rs:185-194`；CLI 无 sync（`main.rs:52-220`） |
| G4 | **索引进度黑盒**——embedding 全异步（写入与 outbox 入队同事务，worker 消费），但 FileInfo/DirEntry 无任何 indexing 状态，用户不知道「什么时候能搜到」 | 批量传 500 个文件后只能盲等/盲试 | `api.rs:112-134`；worker 异步链路 `ARCHITECTURE.md` |
| G5 | **CLI 数据面不认环境变量**——wk_ 只从 config.toml 读，必须先 `veda init`；默认 server 还是 `localhost:3000`。FUSE 反而认 `$VEDA_SERVER`/`$VEDA_KEY`，不对称 | agent/CI/脚本场景没有「一行 export 即用」的路径 | `veda-cli/src/config.rs:64`；`veda-fuse/src/main.rs:40-52,259-273` |
| G6 | **`/v1/answer` 不可发现**——reference.md 无章节、CLI 无 ask 子命令 | 照文档接入的人不知道有一站式带引用问答 | grep `reference.md` 无 answer；CLI 无 ask |
| G7 | search 出处只到 chunk、无行号——`SemanticChunk{index,content}` 生成时就没记行号（行号只在 256KB 存储分块上，与语义 chunk 不对齐） | coding agent 无法精确跳转原文行；grep 可兜底 | `veda-types/src/types.rs:591-595`；`chunking.rs:119-141` |
| G8 | 单 chunk 无邻接上下文；hybrid 无 min_score（RRF 分数不可解释） | 边界片段断章；无法按阈值过滤 | 调研结论 |

## 3. 范围与分期

| 期 | 项 | 一句话 | 状态/前置 |
|---|---|---|---|
| **P0** | veda-server `/mcp` 端点 | Streamable HTTP MCP，只读工具集 6 个，用户侧纯 json 配置零安装 | 纯 server 改动 |
| **P1-1** | CLI 认 `$VEDA_SERVER`/`$VEDA_KEY` | 数据面命令 env 直连，与 FUSE 对齐（CI/脚本场景；HTTP MCP 已不依赖它） | 改动极小 |
| **P1-2** | 索引进度可见性 | workspace 级 pending 计数端点 + CLI 展示 | 无 |
| **P1-3** | answer 可发现性 | reference.md 补章节 + `veda ask` 子命令 | 无 |
| ~~完成~~ | Word 提取 | **已随 0.1.20 上线**（07-22 三节点 + backfill），出方案 | — |
| **P2** | HTML 提取 | 降级（07-22 review）：先用真实语料实测检索质量，数据不行再做 | 见 §6 状态框 |
| **P2** | `veda sync` | 降级（07-22 review）：增量上传已被 sha 短路解决；删除同步先走 FUSE+rsync | 见 §8 状态框 |
| **P2** | 其余背包 | 行号出处 / 邻接上下文 / OCR / min_score / wiki 平台 connector | 看真实反馈 |

**第一批交付 = P0（纯 server 发版，三节点源码 build）**。P1 三项均小、以 CLI 为主，随后一批。

## 4. P0 MCP server（veda-server `/mcp` 端点）

### 4.0 为什么 CLI + skill.md 不够（07-22 review 质疑的正面回答）

已有的 CLI+skill 路径对 Claude Code 用户可用，但有三个结构性短板，MCP 逐条解决：

1. **skill 机制是 Claude Code 私有**。Cursor 没有 skills（rules 文件要手工拷贝维护）、Codex 靠 AGENTS.md——各家私有机制互不通用。公司推广无法约束同事用哪个 agent；**MCP 是唯一被所有主流 Coding Agent 原生支持的接入协议**，这是「公司级最好体验」与「单一 agent 凑合用」的分界线。
2. **结构化 tool call vs 文本指引拼 bash**。skill 链路 = agent 读 SKILL.md → 自己拼 shell 命令 → 转义/引号正确 → 解析文本输出，每步都有失误面；MCP 参数与返回都是 schema 化结构，tool use 是模型训练分布内的一等公民，可靠性高一档。
3. **配置可分发**。`.mcp.json` 提交进项目 git 仓库，团队 clone 即得；远程形态下（见 4.1）连二进制都不用装。

### 4.1 形态选择（07-22 二轮 review 重定：远程端点取代本地子命令）

原理先摆清：`.mcp.json` 的作用是告诉 agent「MCP server 在哪」——agent 与 server 之间说 MCP 协议（JSON-RPC 的 initialize/tools/list/tools/call），veda 现有 REST 不说这门语言，**中间必须有一个翻译进程**。它可以在本地（`command` 字段拉起子进程），也可以在服务端（`url` 字段直连，MCP 2025-03-26 spec 起的 Streamable HTTP transport）。

| 方案 | 用户侧 | 实现 | 取舍 |
|---|---|---|---|
| **A. veda-server 原生 `/mcp` 端点（Streamable HTTP，推荐）** | `.mcp.json` 配 `url` + `Authorization: Bearer wk_`，**零安装、零 init**——「发一段 json + 一个 key」即完成推广 | server 加一个 POST 路由做 stateless JSON-RPC 分发；6 工具**进程内直调 service 层**（SearchService/FsService/AnswerService），不走 HTTP 回环；鉴权复用 `AuthWorkspace` extractor，与 REST 同一道闸 | ✅ 分发体验碾压（Joe 直觉的「配个 json 就行」即此形态）；✅ 集中升级，不追着全员更新二进制；✅ 实现最薄。❌ server 多一个要跟 MCP spec 演进的协议面（tools-only 面很小）；❌ `/mcp` 须挂 30s TimeoutLayer 之外（ask 90s，`/v1/answer` 已有同款处理可抄）；❌ 个别 client 的 remote 支持需真机验证（见验收；不支持者用现成 `mcp-remote` stdio↔HTTP 桥，不自研） |
| B. `veda mcp` 子命令（stdio） | 装 veda 二进制 + json 配 `command` | CLI 子命令走 `client.rs` REST | **已否决（07-22 Joe 拍板：veda 不做 stdio，只支持 Streamable HTTP）**。个别 client 不支持 remote 时用社区 `mcp-remote` 桥（用户侧工具，veda 零代码） |
| C. npm/pip 包装层 | npx 一行 | Node/Python 独立包 | 引入第二语言栈的发布/维护，拒绝 |

**拍板：A。** 会话模型用 **stateless**（6 工具全无状态，每请求独立带 wk_ 鉴权，无 Mcp-Session-Id 状态管理）；v0 每个 POST 直接回单个 JSON 响应，不做 SSE 流式（工具结果无需流，ask 等最终结果即可）。

### 4.2 协议实现

| 方案 | 取舍 |
|---|---|
| 官方 Rust SDK `rmcp` | 带 streamable-http server 侧支持；但其 http 组件是**会话型设计**（session manager/SSE 下行流），stateless tools-only 场景只用到 ~10%，且要与现有 axum 路由/`AuthWorkspace`/超时层编排磨合 |
| **手写 JSON-RPC 路由（实现时拍板）** | stateless tools-only 只需 initialize / tools/list / tools/call 三个方法的 POST 分发 + GET/DELETE 405，几百行、零新依赖；鉴权/错误/超时与现有 server 模式无缝；协议版本自己跟（面小：tools-only 无 capability 变化） |

**拍板（2026-07-22 实现时）：手写。** 理由：为 10% 的用途引入会话型框架违背「有收益证据才上抽象」；stateless 面小到自己跟 spec 的成本低于磨合框架的成本。若后续要上 SSE 下行/会话特性再重估 rmcp。

### 4.3 工具集设计（v0 只读，6 个）

实现为进程内直调 service 层；下表「语义等价端点」列仅标注行为对齐的 REST 参照（错误语义、参数上限与之一致）。

| 工具 | 参数 | 语义等价端点 | 说明 |
|---|---|---|---|
| `search` | `query`（必）, `limit=10`, `path_prefix?`, `detail_level=full` | `POST /v1/search`（hybrid 固定） | 描述里写明 token 经济学：**先 `detail_level=abstract`（~100 tok/hit）筛相关，再 read_file 读原文**——把 L0/L1/L2 的省 token 设计翻译成 agent 行为引导（与 `okf-knowledge-base.md` §5.1 同思路） |
| `grep` | `pattern`（必，字面量）, `path?`, `ignore_case=false`, `limit=100` | `POST /v1/grep` | 唯一带行号的定位工具，描述里写清「literal substring, not regex」 |
| `read_file` | `path`（必）, `offset?`, `limit?`（行范围） | `GET /v1/fs/{path}` | PDF/Word 返回提取文本而非二进制（0.1.20 已具备）；超大文件引导用行范围 |
| `list_dir` | `path=/`, `recursive=false` | `GET /v1/files` | 浏览知识库结构 |
| `overview` | `path`（必） | `GET /v1/overview/{path}` | L1 结构化概览（~2k tok），「读全文太贵、abstract 太薄」的中间档；202 pending 时返回明确提示文本 |
| `ask` | `question`（必）, `path_prefix?` | `POST /v1/answer`（非流式） | 一站式带 `[n]` 引用答案；返回答案正文 + citations（path 列表）。描述写明「复杂/开放问题用 ask，定位具体文件用 search」 |

**设计决策与 trade-off**：

- **只读**。不暴露 write/mkdir/rm。理由：知识库消费场景 agent 只需读；写由管理员走 CLI/sync；误删风险不对称。配套建议：给消费者发 **read-only wk_**（server 权限闸已有），MCP 即使未来加写工具也被 key 挡住——防御两层。
- **hybrid 固定，不暴露 mode 参数**。减少 agent 决策面；semantic/fulltext 的区分对 agent 无感知价值（`score_type` 已在返回里）。反对意见：fulltext 精确短语场景——grep 工具已覆盖。
- **ask 非流式**。MCP tool call 本身是请求-响应模型，流式无意义；`/v1/answer` 90s 兜底 deadline 在 MCP client 默认超时内可配，工具描述注明「可能需要 30-90s」。
- **不做 resources/prompts capability**。v0 只有 tools；MCP resources（把文件树暴露为资源列表）对大知识库是列举放大，等真实需求。

### 4.4 鉴权与配置

- 鉴权 = 请求头 `Authorization: Bearer wk_…`，走现有 `AuthWorkspace`（fs kind 校验、read-only 语义、级联吊销全部继承）；无 header / 坏 key / db-kind key 返回与 REST 一致的错误语义（JSON-RPC error 包装）。**不做 OAuth**——MCP spec 对 remote server 推荐 OAuth，但公司内网 + 可吊销 wk_ 的场景里是过度设计；主流 client 均支持 headers 直配。
- 用户侧配置体验（写进文档的示例）：

```json
// Claude Code: .mcp.json / Cursor: .cursor/mcp.json
{
  "mcpServers": {
    "veda-wiki": {
      "type": "http",
      "url": "https://veda.dbpaas.dingdongxiaoqu.com/mcp",
      "headers": { "Authorization": "Bearer wk_..." }
    }
  }
}
```

- 多知识库：**一个条目绑一个 workspace**（header 里的 key 决定）。要接多个库就配多个条目（`veda-wiki` / `veda-api-docs`），server name 即区分。v0 不做「工具带 workspace 参数」——key 与 workspace 一对一是现有安全模型，不破。
- 网络前提：员工机直连测试/生产域名（TLS 有效），与现有 REST 接入同一路径；mac 代理坑沿用既有接入文档的说明。

### 4.5 验收标准

1. **协议**：MCP Inspector（官方调试工具）连 `POST /mcp` 完成握手，`tools/list` 返回 6 工具且 schema 合法；stateless 模式下并发独立请求互不干扰。
2. **e2e（真实测试环境 MySQL/Milvus/embedding，按测试约定不 mock）**：向 veda_it workspace 传入含中文 sentinel 的 md/pdf/docx 样例 → 经 HTTP `tools/call` 依次验证 search（命中 sentinel）、grep（返回行号）、read_file（docx 返提取文本）、list_dir、overview（ready 或 pending 提示）、ask（返回带 citations 的答案；LLM 未配时 FEATURE_DISABLED 的明确文案）。
3. **真机逐 client 验证**：Claude Code 与 Cursor 各配 `url`+`headers` 完成一次「自然语言提问 → agent 自主调用 search/ask → 回答带知识库出处」（人工验收，transcript 留档）；Codex 验证 remote 支持，若只认 stdio 则文档给出 `mcp-remote` 桥接配置并实测一遍。
4. **超时**：`/mcp` 挂在 30s TimeoutLayer 之外；构造 60s+ 的 ask 调用不被中途砍断；ask 并发复用 `answer_concurrency` 闸（超出返回可读的 busy 错误而非挂起）。
5. **错误路径**：无 Authorization / 坏 key / kind=db 的 key / 不存在的工具名 / 参数校验失败——JSON-RPC error 均可读、HTTP 状态合理、server 不 panic 无资源泄漏。
6. **安全**：`/mcp` 不绕过任何现有鉴权闸（代码评审项）；read-only wk_ 全工具可用（6 工具均只读）；错误信息不泄漏内部路径/SQL。
7. **文档**：`web/public/docs/zh/skill.md` 增「MCP 接入」章节（json 配置示例 + 多库配置 + mcp-remote 桥备注）；`reference.md` 增 `/mcp` 端点说明。
8. 单元测试：JSON-RPC 分发、工具参数校验、错误包装。

## 5. Word 提取（✅ 已完成，出方案）

**已随 0.1.20 上线（2026-07-22）**：双端发版、三节点部署、存量文件 backfill 完成。链路 = `word.rs` 提取（.docx zip+quick-xml / .doc 自写宽松 CFB）、`SourceType::Word` → ExtractSync、`veda_file_extracts` 存提取文本（`source_sha256` 防陈旧）、cat/preview 返提取文本。e2e SOP 见 `docs/word-e2e-sop.md`。本节仅存档，不再是本方案工作项。

## 6. HTML 提取（⬇ P2，07-22 review 降级：先实验后开发）

> **降级理由（Joe review 结论 + 复核认可）**：HTML 是合法 UTF-8，**今天就能入库、能 embed、能被 BM25/语义命中**——「标签噪声拉低检索质量」是推断，未实测。BM25 一路对标签相对鲁棒（标签是英文 token，不与中文查询相撞），受影响更大的只是语义向量与 agent read_file 的 context 开销。
> **触发条件**：真有 HTML wiki 要接时，先做半小时实验——真实 Confluence 导出灌测试库，跑 10 条典型查询实测命中质量 + 抽看 read_file 输出可读性。**实测不行才启动本节开发**，届时按下述已定稿的设计做。

以下为预研设计存档（触发后直接可用）。

### 6.1 怎么识别 HTML

| 方案 | 取舍 |
|---|---|
| **扩展名 `.html`/`.htm`（大小写不敏感）（推荐）** | 可预期、零误判；wiki 导出物必有扩展名。漏无扩展名的 html——本场景不存在 |
| 内容 sniff（`<!DOCTYPE`/`<html` 前缀） | 抓得全；但会误伤「开头贴 html 代码块的 markdown 教程」这类文件，误判成本（错误提取）高于漏判 |

注意实现位置：HTML 是合法 UTF-8，**到不了现有 blob 的 infer magic-byte 分支**（`fs.rs` 的 `detect_mime_and_source` 只对非 UTF-8 走）——检测要加在文本写入路径上按扩展名分流。

### 6.2 提取成什么

| 方案 | 取舍 |
|---|---|
| 裸 strip tags（只留文本） | 最简；但丢标题层级，chunking 退化成纯滑动窗口，检索质量打折 |
| **html → markdown（推荐）** | `<h1>`→`#`：`semantic_chunk` 的 heading 切分（`chunking.rs:32-75`）**直接受益**；链接转 `[text](url)` 为 OKF 方向的关系图（`okf-knowledge-base.md` §7）留了原料。成本：一个转换 crate（候选 `htmd`，纯 Rust；备选 scraper+手写映射，仅 h/p/li/a/table 几类标签即可，Confluence 导出结构固定） |
| LLM 清洗 | 效果上限高但引入成本/延迟/不确定性，**过度设计，拒绝** |

同时 strip `<script>`/`<style>` 内容（Confluence 导出常带), 表格转 markdown 表格（转不动时保底逐行文本）。

### 6.3 走哪条管线

| 方案 | 做法 | 取舍 |
|---|---|---|
| **a. 进 ExtractSync 家族（推荐）** | `SourceType::Html`，**原始 html 存 blob**，worker 提取 markdown 存 `veda_file_extracts` + embed；cat/preview/read_file 返回干净 markdown，原件走 raw 下载 | ✅ 与 PDF/Word 完全同构，复用 Word（0.1.20）铺好的全部基建（extract 表、sha 防陈旧、preview、rewrite 清理）；✅ agent 拿到的是干净文本。❌ html 从「文本」变「blob」：不可 append、grep/SQL `veda_read` 语义变化（改为读提取文本，与 cat 一致） |
| b. 存储不动，embed 前预处理 | 仍走文本路径存原文，仅 chunking 前 strip | 改动最小；但 cat/grep/L0 摘要/SQL 全都继续吃带标签原文——**只治了 embedding 一个症状**，agent read_file 拿到的还是标签汤 |

**拍板建议：a。** wiki 导出的 html 是只读产物，append 语义无人需要；「agent 读到的必须是干净文本」是本场景的硬要求，b 达不到。

### 6.4 验收标准

1. 单测：真实 Confluence 导出页 fixture → 提取结果无 `<` 标签残留、标题映射为 `#`、脚本/样式内容不出现。
2. 集成（真实依赖）：上传 html → search 命中正文关键词且 top hit 的 content 无标签；cat/read_file 返回 markdown；`GET` raw 下载 byte-for-byte 等于原件。
3. 覆盖写 html→image：旧向量被清（复用 Word e2e 同一测试模式，SOP 见 `docs/word-e2e-sop.md`）。
4. 一个 20+ 页的真实 wiki 导出目录 `cp -r` 全量入库，抽 5 页人工检查检索质量。

## 7. P1-1 CLI 认 `$VEDA_SERVER` / `$VEDA_KEY`

- 数据面命令（cp/cat/ls/search/grep/ask/sync/…）读取顺序：**`--server`/`--key` flag > env > config.toml**（与 FUSE 现有顺序一致，`veda-fuse/src/main.rs:143`）。
- 命名沿用 FUSE 的 `$VEDA_SERVER`/`$VEDA_KEY`（=数据面 wk_）；已有的 `$VEDA_API_KEY`（vk_，仅 claim/upgrade 用，`main.rs:1098-1103`）职责不变，文档里把两者的分工写成表格，避免混淆。
- env 模式下不落盘、不改 config.toml；`veda status` 显示凭证来源（env/config），排障时一眼可见。
- 注意事项（实现时验证）：veda-server 的 `VEDA_` 前缀 config 覆盖与 CLI 读的 env 是不同进程不冲突，但同一台 box 上人肉操作时 `VEDA_SERVER` 可能被误解——文档注明该变量仅 CLI/FUSE 客户端读取（HTTP MCP 不读本地 env，key 在 `.mcp.json` header 里）。

**验收**：干净机器（无 config.toml）`export VEDA_SERVER=… VEDA_KEY=…` 后 `veda search`/`veda ls` 直接可用；flag 覆盖 env、env 覆盖 config 的三层优先级各一条测试；`veda status` 显示来源。

## 8. `veda sync`（⬇ P2，07-22 review 降级）

> **降级理由（Joe review 结论 + 复核认可）**——拆开看两个子需求：
> - **增量上传：已被现有机制解决。** `cp -r` 重跑即增量——客户端带 `If-None-Match: sha256`（`client.rs:185-194`），服务端对未变文件 content_unchanged 短路，不重复 embed。代价只是全量读盘+全量 PUT body 的带宽（内网万级文件≈分钟级），不是痛点。原方案把它做成独立命令属于 gold-plating。
> - **删除同步：需求真实但有零开发替代。** 先澄清边界（07-22 Joe 问）：veda 侧的 embedding 生命周期是**全自动正确**的——覆盖写重 embed（幂等 upsert + `delete_chunks_above` 清尾 + content hash 水印防重复），删除经 `ChunkDelete` 清 Milvus 向量，均与写/删同事务入队。所以**修改场景 `cp -r` 重跑即完备**。缺口只在删除的**触发**：`cp -r` 只遍历本地现存文件，本地删掉的页不会对远端发 DELETE，远端文件连同 embedding 原样留存（没人叫它删，不是它不会删）→ 被持续检索命中。替代 = 手动 `veda rm`（低频够用）或 **FUSE 挂载 + `rsync --delete <local>/ <mnt>/<remote>/`**——rsync 算出该删谁，FUSE unlink 下发 DELETE。性能弱于原生 sync（逐文件穿 HTTP），管理员低频操作可接受。
> - **「实时同步」语义澄清**：veda 体系内已是自动的——上传后 chunking/embedding 异步自动（秒~分钟可搜）、FUSE 写入即上传、SSE 变更即推。不自动的只有「源头（Confluence/本地）→ veda」这一跳，源头在 veda 体系外，必须有触发（人工 cp / CI cron / FUSE+rsync）。要 veda 主动订阅 wiki 平台变更 = connector，每平台一个适配器，进方向池不排期。
> **触发条件**：需求方真实使用后，更新/删除频率高到「cp 重跑 + FUSE rsync」组合成为明确痛点（有使用数据佐证），再启动本节。

以下为预研设计存档（触发后直接可用）。

**语义：单向 push**，`veda sync <local-dir> <remote-dir>`，以本地为真相源（wiki 源头在 git/本地导出）。不做双向——「远端当本地读」已由 FUSE 覆盖，双向冲突解决是另一个量级的问题，本场景不需要。

### 8.1 diff 依据

| 方案 | 做法 | 取舍 |
|---|---|---|
| **a. 远端对比（推荐）** | 一次递归 list 拿远端 `(path, checksum)`，本地 walk 算 sha256，diff 出 新增/变更/删除 三集合 | ✅ 无本地状态文件，换机器/多人操作结果一致；✅ 服务端 checksum 是写入时算好的（sha256 dedup 机制现成）。❌ **前置小改**：`DirEntry` 目前没有 checksum 字段（`api.rs:126-134`，只有单文件 stat 的 `FileInfo` 有，`api.rs:120`）——递归 list 响应加 `checksum`，数据在 files 表现成，服务端一行级改动；❌ 本地全量读盘算 sha（万级文件规模无压力，实测再说） |
| b. 本地 manifest | `.veda-sync.json` 存上次 (path, sha, mtime)，mtime+size 预筛 | 不打 list、不读全盘；但状态漂移（他人/别机传过）会导致漏传或误判，多了一个要维护的状态文件 |

**拍板建议：a。** b 的 mtime 预筛留作性能优化选项（真实规模遇到瓶颈再加，先不做）。

### 8.2 行为设计

- **上传**：只传 新增+变更（sha 不同）；沿用 `cp -r` 的 ignore 列表与失败容错（单文件跳过、连续 10 失败中止）。
- **删除**：`--delete` 显式 opt-in（rsync 惯例）。删除前把清单打到 stderr 并交互确认；`--non-interactive` 或 CI 环境下直接执行但打印计数。**安全阀**：待删数 > 远端现存文件数的 50% 时强制要求交互确认或 `--force`——防「本地目录传错了」把整库清空。
- **dry-run**：`--dry-run` 输出三集合计数与清单，不动数据。
- 结束输出：`uploaded N, unchanged M, deleted K (P files queued for indexing)`——最后一段接 §9 索引可见性。
- 远端 list 上限：单 workspace 递归 100k 条（`MAX_RECURSIVE_DESCENT`），wiki 规模（千~万页）余量充足；超限直接报错引导拆 workspace，不做静默截断。

### 8.3 验收标准

1. 集成（真实依赖）：首跑=全量上传；改 1 个文件重跑=只传 1 个（网络请求数断言）；本地删 2 个 + `--delete`=远端删 2 个；`--dry-run` 零副作用。
2. 安全阀：构造「本地只剩 10%」场景，非交互下拒绝执行。
3. 幂等：sync 中途 kill，重跑收敛到一致（diff 为空）。
4. 规模：1000+ 文件目录跑通，耗时可接受（记录基线数据进 plan 回写）。
5. `DirEntry.checksum` 字段：web console / FUSE 等现有消费者不受影响（加字段是兼容变更，但按项目「可自由打破」约定无需特殊处理，验一下编译面即可）。

## 9. P1-2 索引进度可见性

### 9.1 方案

| 方案 | 取舍 |
|---|---|
| **a. workspace 级计数端点（推荐）** | `GET /v1/index-status` → `{pending, processing, dead}`，按 workspace 查 outbox 表 count。**只统计决定可搜索性的 `ChunkSync`/`ExtractSync`**（SummarySync 有 30s debounce + burst window，计入会长期非零造成误导；L0/L1 滞后不影响 L2 检索）。轻：一条 count SQL + 合适索引 |
| b. per-file 状态字段 | FileInfo 加 `index_state`——最精确，但 list 要 join outbox（读放大）、契约变更大、且「哪个文件还没好」在批量场景不是真问题（用户要的是「都好了没」） |
| c. 客户端 search 试探 | 无服务端改动，但丑且不可靠（search 命不中 ≠ 没索引） |

**拍板建议：a。** `dead > 0` 时 CLI 显式提示「N 个文件索引失败，联系管理员看 outbox 死信」——把 2026-06-12 outbox 死信事故的教训（问题要响亮暴露）落进用户面。

### 9.2 CLI 集成

- `veda cp -r` / `veda sync` 结束打一行：`K files queued for indexing (check: veda status --index)`。
- `veda status --index`：显示三计数；`--wait` 轮询（5s 间隔）到 pending+processing=0 退出，退出码 0；有 dead 退出码非 0——CI 可用「sync && veda status --index --wait」做「传完且可搜」的门。

### 9.3 验收标准

1. 集成：传 50 文件，端点计数从 N 单调降到 0，`--wait` 正常退出；注入一条必失败任务（如超大 chunk 触发 embedding 400），dead 计数可见且退出码非 0。
2. 端点鉴权：`AuthWorkspace`（wk_ 可查自己 workspace），不暴露跨租户信息。
3. 性能：outbox 表按 `(workspace_id, status)` 的 count 走索引（EXPLAIN 验证），不做全表扫。

## 10. P1-3 `/v1/answer` 可发现性

两个动作：

1. **`reference.md` 补 answer 章节**：请求/响应结构（含 `citations` 的 `{index, path, spans}` 语义、`spans=[]`=整文件）、SSE 五事件表（delta/reset/tool/final/error，final 权威）、错误码（429 并发满=`answer_concurrency` 默认 2 / 501 未配 LLM / 90s deadline）、`prompt` 与 `path_prefix` 参数。对外权威文档是 web zh docs——同步。
2. **`veda ask` 子命令**：`veda ask "问题" [--path PREFIX] [--json]`。v0 非流式（`POST /v1/answer`）：打印答案正文 + 出处列表（每条一行 path）；`--json` 输出原始 `AnswerApiResponse` 供脚本/agent 解析；429/501 给明确话术与独立退出码（沿用 abstract 的 2/3 模式设计一致性）。终端流式（SSE 逐字）留 P2——先解决「存在性」，再解决「体验」。

**验收**：`veda ask` 在测试环境返回带引用答案；`--json` 可被 `jq .data.citations` 解析;501（未配 LLM 的 server）/429 文案正确、退出码可区分;reference.md 章节经 web 构建渲染检查;skill.md 的决策表加一行「复杂问题 → veda ask」。

## 11. P2 背包（不排期，触发条件写明）

| 项 | 内容 | 触发条件 |
|---|---|---|
| search 出处行号 | `SemanticChunk` 加行号需改 chunking 记偏移 + Milvus schema 加字段 + 全量重刷（`types.rs:591-595` 现无行号，**不是**「把存的行号返回」——存储分块的行号与语义 chunk 不对齐） | MCP 用起来后，agent 出现明确的「精确跳转」需求且 grep 兜底不够 |
| 邻接上下文 | search 响应可选 `context=N` 带前后 chunk | qa_log / 用户反馈出现「片段断章」案例 |
| OCR | 扫描 PDF / 图片文字（ARCHITECTURE 待实现项） | wiki 里真出现扫描件占比 |
| fs search `min_score` | 仅 semantic/fulltext 可做（hybrid RRF 分数无绝对语义，db vectors 侧已是此约束） | agent 出现「低质命中污染上下文」反馈 |
| `veda ask` 流式输出 | SSE 渲染到终端 | 有人真在终端里当聊天用 |
| HTML 提取 | 设计存档于 §6 | 真实 HTML 语料实测检索质量不达标（§6 状态框） |
| `veda sync` | 设计存档于 §8（含前置 DirEntry.checksum） | 「cp 重跑 + FUSE rsync --delete」组合成为实测痛点（§8 状态框） |
| wiki 平台 connector | veda 主动订阅 Confluence 等平台变更（webhook/轮询），每平台一个适配器 | 「人工导出→上传」的触发模式被证明不可持续；量级大，进方向池 |

## 12. 明确不做（本方案范围内）

| 项 | 理由 |
|---|---|
| OpenAPI / tool schema 端点 | agent 面由 MCP + skill.md 覆盖，codegen 无真实消费者；`reference.md:355` 维持「无 OpenAPI」现状 |
| cross-encoder / LLM rerank | hybrid RRF 未被证明不够；等 qa_log 出现召回质量证据再议（有 tunnel qa 遥测基建可依赖） |
| OnePaaS Python skill 推进 | 独立轨道（`veda-skill` 仓库，沙箱场景），与本方案不互斥；平台侧有真需求再动 |
| 双向 sync / pull 模式 | wiki 真相源在本地/git；「远端当本地」由 FUSE 覆盖 |
| workspace 存储配额 | 公司内部信任模式 + 50MiB/文件上限够挡意外;真滥用再做（简化偏好：能不上就不上） |
| MCP resources capability | 文件树当 MCP 资源列举，对大库是放大器;tools 里的 list_dir 够用 |

## 13. 端到端验收（整体 DoD，全部真实依赖）

**场景剧本**（测试环境完整走一遍，作为发布 gate）：

1. **A（管理员）**：拿到读写 wk_ → `export VEDA_SERVER=… VEDA_KEY=…`（零 config.toml）→ `veda cp -r ./wiki-export /wiki`（含 md + docx + pdf 各若干真实页）→ 输出 queued 计数 → `veda status --index --wait` 退出 0。
2. **维护路径**：改 1 页重跑 `cp -r` → 服务端 content_unchanged 短路其余文件（日志/计数佐证）；删 1 页走 FUSE + `rsync --delete`（或 `veda rm`）→ 被删页 search 不再命中（无孤儿）。
3. **B（开发）**：Claude Code 配 MCP（`.mcp.json` 里 url + Bearer read-only wk_，**本机零安装**）→ 自然语言问知识库内的业务问题 → agent 自主调用 search/ask → 回答带知识库文件出处;全程 B 未读过任何 veda 文档、未装任何 veda 二进制。
4. **格式质量**：每种格式抽 1 页人工验证：search top-3 命中正确页、read_file 内容干净（无乱码）、中文语义查询可命中英文关键词页的反向亦然。若需求方语料含 HTML，此步顺带完成 §6 的质量实验并记录数据。
5. **回归**：38 个集成测试 binary 基线全绿（`--test-threads=1` + `NO_PROXY='*'`）;现有 CLI/FUSE/web console 行为无回归。
6. **文档完备**：reference.md（answer + index-status）、skill.md（MCP 章节）、CHANGELOG、ARCHITECTURE.md 同步;本 plan 按实际偏差回写后归档。

## 14. 开放问题（需要 Joe / 需求方拍板）

1. **对方 wiki 的导出格式是什么？**（Markdown / Confluence HTML / docx / 混合）——含 HTML 则跑一次 §6 的半小时质量实验（灌真实语料测 10 条查询），用数据决定 HTML 提取是否触发。
2. **对方 wiki 的更新/删除频率？**——决定 §8 sync 的触发时机；低频维护则「cp 重跑 + FUSE rsync --delete」长期够用。
3. 消费者 key 策略：按 §4.3 建议「B 一律 read-only wk_」，是否需要管理端批量发 key 的便利（现有 console 已可发，暂判够用）？
4. 发版节奏：P0（`/mcp`）是纯 server 改动——CI 不发 server，三节点源码 build 窗口安排；P1 三项以 CLI 为主随下一个 CLI 版本（0.1.21？）。
5. MCP 工具名/参数名发布后即是对外契约，v0 定稿前是否请一位真实用户（需求方同事）过一遍工具描述？
