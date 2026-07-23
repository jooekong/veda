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
    "veda-kb": {
      "type": "http",
      "url": "https://veda.ddmc-inc.com/mcp",
      "headers": { "Authorization": "Bearer wk_..." }
    }
  }
}
```

**Which config level?** Pick by who the knowledge base belongs to:

- **User level (recommended for a cross-project company KB)** — the KB follows the person, not any single repo; configure once, every project gets it:

  ```bash
  claude mcp add --transport http --scope user veda-kb \
    https://veda.ddmc-inc.com/mcp \
    --header "Authorization: Bearer wk_..."
  ```

  Cursor's global config lives at `~/.cursor/mcp.json` (same JSON as above).

- **Project level** — when one project binds its own dedicated KB, commit the `.mcp.json` above into the repo; teammates get it on clone.

The agent then discovers six read-only tools — no prompt engineering needed:

`search` (hybrid semantic+keyword, tiered L0/L1/L2 detail) · `grep` (literal, line numbers) · `read_file` (PDF/Word return extracted text) · `list_dir` · `overview` (L1 structured summary) · `ask` (one-shot RAG answer with `[n]` citations)

Notes:

- **One entry binds one workspace** (the key decides). Multiple knowledge bases = multiple entries with distinct names (`veda-kb` / `veda-api-docs`).
- `.mcp.json` can live in your project's git repo so teammates get it on clone (inject the key via env, don't commit it).
- Tools are read-only — content maintenance goes through the CLI path below.
- **Fewer permission prompts**: since every tool is read-only, it's safe to allowlist `mcp__veda-kb__*` in Claude Code's `permissions.allow`; newer servers also declare `readOnlyHint` per the MCP spec, and clients that honor it relax confirmation automatically.
- For stdio-only clients, bridge with the community [`mcp-remote`](https://www.npmjs.com/package/mcp-remote) package.
- Protocol details: see the MCP section in the [API reference](#/docs/reference).

### Teach the agent when to look (drop into your project's CLAUDE.md / AGENTS.md template)

Mounting the tools is step one — the agent also needs to know **when to query**. Paste this into the project's agent-guidance file:

> Before acting, check the company knowledge base (veda-kb MCP tools) whenever the task involves: interfaces, conventions, or deployment of **other teams / other repos**; internal middleware usage or ops SOPs; "why is this system designed this way" questions. Usage: `search` first (`detail_level='abstract'` is the cheap relevance scan), then `read_file` the promising paths; use `grep` for exact identifiers; use `ask` only when you want a synthesized, cited answer (slow, 10-90s). **For this repo's own code and docs, read locally — don't query the KB.**

That last boundary matters: without the "local vs cross-project" line, agents either never query or query for everything.

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
