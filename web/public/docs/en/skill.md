# AI agent integration (MCP / Skill)

Two ways to wire an AI tool to veda — pick by need:

| Path | Use when | Install |
|---|---|---|
| **MCP (recommended)** | A coding agent needs to **query** the knowledge base (search / read / Q&A); works natively in Claude Code / Cursor / Codex | **Zero install** — one JSON block |
| CLI + skill.md | You need **writes** (upload / delete / maintenance), scripting, or the full FUSE/SQL surface | Install the `veda` CLI |

---

## MCP (recommended, zero install)

veda-server natively serves an MCP endpoint (`POST /mcp`, Streamable HTTP transport, protocol `2025-06-18`). All you need is a fs-workspace `wk_` (ask your admin for a **read-only** one) and one config block:

```json
// Claude Code: .mcp.json at project root / Cursor: .cursor/mcp.json
{
  "mcpServers": {
    "veda-wiki": {
      "type": "http",
      "url": "https://<your-veda-host>/mcp",
      "headers": { "Authorization": "Bearer wk_yourkey" }
    }
  }
}
```

The agent then discovers six read-only tools — no prompt engineering needed:

`search` (hybrid semantic+keyword, tiered L0/L1/L2 detail) · `grep` (literal, line numbers) · `read_file` (PDF/Word return extracted text) · `list_dir` · `overview` (L1 structured summary) · `ask` (one-shot RAG answer with `[n]` citations)

Notes:

- **One entry binds one workspace** (the key decides). Multiple knowledge bases = multiple entries with distinct names.
- `.mcp.json` can live in your project's git repo so teammates get it on clone (inject the key via env, don't commit it).
- Tools are read-only — content maintenance goes through the CLI path below.
- For stdio-only clients, bridge with the community [`mcp-remote`](https://www.npmjs.com/package/mcp-remote) package.
- Protocol details: see the MCP section in the [API reference](#/docs/reference).

---

## Claude Code (CLI + skill, when you need writes)

`install.sh` detects `~/.claude` and drops the skill into Claude Code's skill directory automatically:

```bash
curl -fsSL https://veda.ddmc-inc.com/install.sh | sh
```

Verify:

```bash
ls ~/.claude/skills/veda/SKILL.md      # exists
```

Next time you ask Claude Code to upload / search files, it auto-loads this skill and calls the `veda` CLI.

---

## Cursor / Continue.dev / other rule-file tools

These typically have project-level rules or a global system prompt. Paste skill.md content in.

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

Just include skill.md content as part of your `system_message`. Fetch the latest at agent boot:

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
curl -fsSL https://veda.ddmc-inc.com/install.sh | sh
# Re-installs CLI + rewrites ~/.claude/skills/veda/SKILL.md
```

---

## See what's currently in the skill

```bash
curl -fsSL http://git.ddxq.mobi/middleware/dbpaas/veda/-/raw/main/skill.md | less
```

Or just open the GitLab rendered version in your browser.
