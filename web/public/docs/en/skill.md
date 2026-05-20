# AI agent integration (Skill)

Veda ships an agent-facing [`skill.md`](http://git.ddxq.mobi/middleware/dbpaas/veda/-/raw/main/skill.md) — a single markdown file an LLM can read to know:

- How to onboard / init
- How to use each command (files, search, collections, SQL)
- Decision tables: when to use `search` vs `grep`, when to FUSE-mount
- Error-code → action mappings
- What **not** to do (so the agent doesn't go off the rails)

Below is how to install it into different AI tools.

---

## Claude Code (auto)

`install.sh` detects `~/.claude` and drops the skill into Claude Code's skill directory automatically:

```bash
curl -fsSL https://veda.dbpaas.dingdongxiaoqu.com/install.sh | sh
```

Result:

```
~/.claude/skills/veda/SKILL.md
```

Next time you ask Claude Code anything about Veda (upload, search notes, etc.) it auto-loads this skill and calls the `veda` CLI correctly.

**Verify:**

```bash
ls ~/.claude/skills/veda/SKILL.md      # exists
```

Then ask Claude Code:

> Upload ~/work/notes/today.md to veda under /notes/

Claude will run `veda cp ~/work/notes/today.md /notes/today.md` instead of hand-rolling HTTP.

---

## Cursor / Continue.dev / other rule-file tools

These typically have project- or global-rules files. Paste skill.md content in.

**Cursor:**

1. Create `.cursor/rules/veda.md` in your project root:

   ```markdown
   # Veda usage rules
   
   When the user mentions veda, file uploads, knowledge base, or semantic search, follow these rules:
   <paste the contents of skill.md here>
   ```

2. Or fetch it in one shot:

   ```bash
   curl -fsSL http://git.ddxq.mobi/middleware/dbpaas/veda/-/raw/main/skill.md \
     -o .cursor/rules/veda.md
   ```

**Continue.dev:** add skill.md's content to the `systemMessage` field in `~/.continue/config.yaml`.

---

## Codex CLI / generic LLM CLI

```bash
mkdir -p ~/.codex/skills
curl -fsSL http://git.ddxq.mobi/middleware/dbpaas/veda/-/raw/main/skill.md \
  -o ~/.codex/skills/veda.md
```

Have the agent `cat ~/.codex/skills/veda.md` into its system prompt.

---

## Custom RAG agent

Just include skill.md content as part of your `system_message`. Inside it you get:

- Decision trees (when to use which command)
- Error-code lookup tables
- "Don't do" list

This alone makes a tool-using LLM dramatically less likely to invent broken commands.

Fetch the latest at agent boot:

```python
import requests
SKILL_URL = "http://git.ddxq.mobi/middleware/dbpaas/veda/-/raw/main/skill.md"
SYSTEM_PROMPT = "You have access to the `veda` CLI. " + requests.get(SKILL_URL).text
```

---

## Narrowing the skill

To give an agent a **narrower** skill (read-only, or scoped to one workspace), copy skill.md, strip the sections you don't want, maintain your own:

```bash
mkdir -p ~/.claude/skills/veda-readonly
curl -fsSL http://git.ddxq.mobi/middleware/dbpaas/veda/-/raw/main/skill.md \
  | grep -v "veda rm\|veda mv\|veda cp \|veda mkdir" \
  > ~/.claude/skills/veda-readonly/SKILL.md
```

> `grep -v` is illustrative; in practice edit the markdown by hand so you don't strip explanation paragraphs.

---

## Does the skill content change?

Yes. `skill.md` lives in the GitLab repo and ships with each Veda release (new commands, new behavior, new error codes). Re-fetch when you upgrade the CLI:

```bash
curl -fsSL https://veda.dbpaas.dingdongxiaoqu.com/install.sh | sh
# Re-installs CLI + rewrites ~/.claude/skills/veda/SKILL.md
```

---

## See what's currently in the skill

```bash
curl -fsSL http://git.ddxq.mobi/middleware/dbpaas/veda/-/raw/main/skill.md | less
```

Or just open the GitLab rendered version in your browser.
