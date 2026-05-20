# AI 助手集成（Skill）

Veda 把 CLI 用法整理成了一份 [`skill.md`](http://git.ddxq.mobi/middleware/dbpaas/veda/-/raw/main/skill.md)：用法、决策表、错误码、不能做的事。下面是怎么把它装到不同的 AI 工具里。

---

## Claude Code（最简单，自动）

`install.sh` 检测到 `~/.claude` 存在时，会自动把 skill 装到 Claude Code 的 skills 目录：

```bash
curl -fsSL https://veda.dbpaas.dingdongxiaoqu.com/install.sh | sh
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
   <在此粘贴 https://veda.dbpaas.dingdongxiaoqu.com/install.sh 输出里那份 skill.md 的内容>
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
curl -fsSL https://veda.dbpaas.dingdongxiaoqu.com/install.sh | sh
# 自动重装 CLI + 重写 ~/.claude/skills/veda/SKILL.md
```

---

## 想看现在 skill 里写了什么

```bash
curl -fsSL http://git.ddxq.mobi/middleware/dbpaas/veda/-/raw/main/skill.md | less
```

或者直接打开浏览器看 GitLab 上的 markdown 渲染版本。
