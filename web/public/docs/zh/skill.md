# AI 助手集成（MCP / Skill）

给 AI 工具接 veda 有两条路,按需选:

| 路径 | 适用 | 安装 |
|---|---|---|
| **MCP(推荐)** | 在 Coding Agent 里**查**知识库(检索/读文件/问答),Claude Code / Cursor / Codex 全通用 | **零安装**,配一段 JSON |
| CLI + skill.md | 需要**写**(上传/删除/目录维护)、脚本化、或用 FUSE/SQL 全量能力 | 装 `veda` CLI |

---

## MCP 接入（推荐,零安装）

veda-server 原生提供 MCP 端点(`POST /mcp`,Streamable HTTP,协议 2025-06-18)。你只需要一个 fs workspace 的 `wk_`(建议找管理员要**只读** key),在 agent 的 MCP 配置里加一段:

```json
// Claude Code: 项目根 .mcp.json / Cursor: .cursor/mcp.json
{
  "mcpServers": {
    "veda-kb": {
      "type": "http",
      "url": "https://veda.ddmc-inc.com/mcp",
      "headers": { "Authorization": "Bearer wk_..." }
    }
  }
}
```

**配置放哪一级?** 按知识库归属选:

- **用户级(跨项目的公司知识库推荐)**——知识库跟人走、不属于某个项目,配一次所有项目可用:

  ```bash
  claude mcp add --transport http --scope user veda-kb \
    https://veda.ddmc-inc.com/mcp \
    --header "Authorization: Bearer wk_..."
  ```

  Cursor 的全局配置在 `~/.cursor/mcp.json`(内容同上面的 JSON)。

- **项目级**——某个项目要绑定专属知识库时,用上面的 `.mcp.json` 提交进仓库,团队 clone 即得。

配好后 agent 自动获得这几个只读检索工具,不需要任何提示词教学:

`search`(混合语义+关键词检索,支持 L0/L1/L2 分层省 token)· `grep`(字面量定位,带行号)· `read_file`(PDF/Word 返回提取文本)· `list_dir` · `overview`(L1 结构化概览)· `ask`(一站式 RAG 问答,带 `[n]` 引用出处)

要点:

- **一个条目绑一个 workspace**(key 决定)。接多个知识库就配多个条目,起不同名字(`veda-kb` / `veda-api-docs`)。
- `.mcp.json` 可以提交进项目 git 仓库,团队 clone 即用(key 建议走各自环境注入,不要把 key 提交进库)。
- 检索工具全只读——上传维护知识库内容走下方的 CLI 路径。例外是 agent memory 那组:`memory_save` / `memory_update` / `memory_delete` 会写(`memory_context` / `memory_search` 仍是只读)。
- **减确认弹窗**:每个工具都按 MCP 规范声明了自己的 `readOnlyHint`,支持该标注的 client 会对只读工具自动放宽确认。把 `mcp__veda-kb__*` 加进 Claude Code settings 的 `permissions.allow` 依然省事,只是这个通配也一并放行了上面三个写操作。
- 个别只支持 stdio 的老工具,用社区 [`mcp-remote`](https://www.npmjs.com/package/mcp-remote) 桥接到同一个 url 即可。
- 端点协议细节见 [API 参考的 MCP 章节](#/docs/reference)。

### 让 agent 知道何时来查（建议放进项目 CLAUDE.md / AGENTS.md 模板）

工具挂上只是第一步——agent 还要知道**什么时候该查**。把下面这段放进项目的 agent 指引文件即可直接用:

> 遇到以下情况,先用 veda-kb 的 MCP 工具查公司知识库再动手:涉及**其他团队 / 其他仓库**的接口、约定、部署方式;公司内部中间件用法、运维 SOP;「这个系统为什么这样设计」类问题。用法:先 `search`(`detail_level='abstract'` 便宜地筛相关)再 `read_file` 读原文;查精确标识符用 `grep`;需要一段带出处的结论才用 `ask`(慢,10-90s,少用)。**本仓库自己的代码和文档直接读本地,不查知识库。**

最后一句的边界很重要:不划「本地 vs 跨项目」这条线,agent 要么不查、要么什么都查。

---

## Claude Code（CLI + skill,需要写操作时）

`install.sh` 检测到 `~/.claude` 存在时，会自动把 skill 装到 Claude Code 的 skills 目录：

```bash
curl -fsSL https://veda.ddmc-inc.com/install.sh | sh
```

装好后：

```bash
ls ~/.claude/skills/veda/SKILL.md      # 验证存在
```

下次让 Claude Code 上传 / 搜索文件，它会自动加载这份 skill 并调用 `veda` CLI。

---

## Cursor / Continue.dev / 其他基于规则文件的工具

这类工具通常有"项目级 rules"或"全局 system prompt"。把 skill.md 内容粘进去：

**Cursor**：

1. 项目根目录新建 `.cursor/rules/veda.md`
2. 把以下内容贴进去：

   ```markdown
   # Veda usage rules
   
   When the user mentions veda, file uploads, knowledge base, or semantic search, refer to:
   <在此粘贴 https://veda.ddmc-inc.com/install.sh 输出里那份 skill.md 的内容>
   ```

3. 也可以直接：

   ```bash
   curl -fsSL http://git.ddxq.mobi/middleware/dbpaas/veda/-/raw/main/skill.md \
     -o .cursor/rules/veda.md
   ```

**Continue.dev**：把 skill.md 的内容加进 `~/.continue/config.yaml` 的 `systemMessage` 字段。

---

## Codex CLI / 通用 LLM CLI

```bash
mkdir -p ~/.codex/skills
curl -fsSL http://git.ddxq.mobi/middleware/dbpaas/veda/-/raw/main/skill.md \
  -o ~/.codex/skills/veda.md
```

让 agent 在系统 prompt 里 `cat ~/.codex/skills/veda.md` 加载即可。

---

## 自己写 RAG agent

直接把 skill.md 内容作为 `system_message` 的一部分。也可以让 agent 启动时自动 fetch 最新版：

```python
import requests
SKILL_URL = "http://git.ddxq.mobi/middleware/dbpaas/veda/-/raw/main/skill.md"
SYSTEM_PROMPT = "You have access to the `veda` CLI. " + requests.get(SKILL_URL).text
```

---

## 自定义 / 收窄 skill

如果你想给 agent 一个**收窄版** skill（比如只允许读不允许写，或限定某个 workspace），复制一份 skill.md，删掉不该用的命令章节，自己维护：

```bash
mkdir -p ~/.claude/skills/veda-readonly
curl -fsSL http://git.ddxq.mobi/middleware/dbpaas/veda/-/raw/main/skill.md \
  | grep -v "veda rm\|veda mv\|veda cp \|veda mkdir" \
  > ~/.claude/skills/veda-readonly/SKILL.md
```

> 这里 `grep -v` 只是示意，真要做收窄建议手动改 markdown，避免误伤。

---

## skill 内容会更新吗

会。`skill.md` 跟 CLI 一起在 GitLab 仓库里维护，每次 Veda 发版会更新（新命令、新行为、新错误码）。建议每次 CLI 升级一起重 fetch 一份：

```bash
curl -fsSL https://veda.ddmc-inc.com/install.sh | sh
# 自动重装 CLI + 重写 ~/.claude/skills/veda/SKILL.md
```

---

## 想看现在 skill 里写了什么

```bash
curl -fsSL http://git.ddxq.mobi/middleware/dbpaas/veda/-/raw/main/skill.md | less
```

或者直接打开浏览器看 GitLab 上的 markdown 渲染版本。
