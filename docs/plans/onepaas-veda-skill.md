# veda 接入 OnePaaS AI（Plugin/Skill 市场 + Skill 沙箱）方案

> 目标：让 OnePaaS AI 工作台上的 agent 能用上 veda 的检索/文件/SQL 能力。
> 调研日期 2026-06-25，基于站上 Skill 沙箱文档 + `cs-oss/skill-dev` 的 `llms.txt` 规范。

## 0. 一句话结论

- **接入形态**：发一个 **Claude Plugin 仓库**到 Plugin/Skill 市场，里面是一个 **薄 Python skill 调 veda REST**；在 **Skill 沙箱**里选中它、注入 `wk_`，agent 通过沙箱 **Open API** 调用。
- **不要**把 `veda` CLI 原生二进制塞进去，**更不要**碰 FUSE —— 见 §1 的平台约束。
- 这是"先跑通流程"的最简形态；长期更原生的姿势是 **MCP 网关 / 向量存储后端**（§6）。

> **决定（2026-06-25）**：独立 `veda-skill` 仓库，**不**并入 dbpaas-skills；v0 走 Python 脚本（自包含、纯 stdlib），MCP server 列为 phase 2。
> **骨架已生成**：`~/code/personal/veda-skill/`（plugin.json + marketplace.json + skills/veda/SKILL.md + scripts/veda_client.py），已通过语法/JSON/frontmatter 校验。剩余见 §8。

## 1. 平台约束（决定一切的两条事实）

调研 `llms.txt` + 市场列表 + 沙箱文档得到的硬事实：

1. **运行时是 Python / Node，依赖从源码推断。** 原文：平台"基于 skill 目录下的实际源码做分析（源码扫描、AST、语义分析）……构建 Python / Node.js 等运行环境与安装依赖"。市场里所有 skill 的运行环境列都是 `python@x` / `nodejs@x`，**没有原生二进制（Rust）这一档**。
   - → veda CLI 是 Rust 二进制，不是一等公民。veda 本质是 HTTP 服务，**薄 Python 脚本直接打 REST 才贴平台体质**（市场里的 `weather` 调 Open-Meteo 就是这个套路）。
2. **分发单元是 Claude Plugin，不是单个 skill。** 平台只订阅你的 **Git 仓库**自动生成 marketplace 记录；一个 plugin 透出几个 skill 取决于 `skills/` 下有几个合法子目录。`marketplace.json` 不用自己写。

附带结论：**FUSE 在这个模型里没有位置**——沙箱是"SKILL.md + 脚本经 HTTP 无状态调用"，没有交互式终端、没有 `/dev/fuse`、生命周期是临时的。FUSE 留给真人/IDE 在真机用。

## 2. 形态选择（已拍板 B）

| 方案 | 做法 | 取舍 |
|---|---|---|
| A. 装 veda CLI | 仓库 vendor 预编译二进制 / build 步跑 `install.sh`，脚本 shell 调 CLI | 复用 CLI 全部能力，但**逆平台 Python/Node 体质**，二进制能否在沙箱跑需确认，构建脆 |
| **B. 薄 REST skill（推荐）** | Python 脚本读 `VEDA_KEY`/`VEDA_SERVER`，直接打 veda REST | 贴平台原生、零二进制、依赖只一个 `requests`（甚至 stdlib）；只覆盖 agent 真正要的子集 |

veda 的 REST 面已经是权威（CLI 自己就是它的客户端，见 `crates/veda-cli/src/client.rs`），B 不存在"重写一套逻辑"的负担，只是把 search/grep/cp/cat/ls/sql 几个调用包成脚本。

## 3. 仓库结构（按 llms.txt 的最小交付）

新建一个 Git 仓库（建议 `middleware/dbpaas/veda-skill` 或 `cs-oss/veda-skill`）：

```text
veda-skill/
├── .claude-plugin/
│   └── plugin.json
└── skills/
    └── veda/                     # 目录名必须 == SKILL.md 的 name
        ├── SKILL.md
        ├── requirements.txt      # requests （或纯 stdlib urllib 则省略）
        └── scripts/
            └── veda_client.py     # 薄 REST 封装
```

### 3.1 `.claude-plugin/plugin.json`

严格按官方字段类型（类型写错平台/Claude Code 直接判 manifest 无效）：

```json
{
  "$schema": "https://json.schemastore.org/claude-code-plugin-manifest.json",
  "name": "veda",
  "version": "0.1.0",
  "description": "Veda knowledge store: semantic + fulltext search, file ops, structured collections, and SQL over a workspace.",
  "author": { "name": "Joe Kong", "email": "jookooong@gmail.com" },
  "repository": "https://git.ddxq.mobi/middleware/dbpaas/veda-skill.git",
  "license": "MIT",
  "keywords": ["veda", "vector-search", "rag", "knowledge-base", "paasai"],
  "skills": "./skills/"
}
```

要点：`author` 是 object 不是字符串；`skills` 是路径字符串（或数组）；**不要**写 `interface` / `capabilities` / `pluginApiVersion`（非官方字段）。

### 3.2 `skills/veda/SKILL.md`（frontmatter 按 agentskills spec）

```yaml
---
name: veda
description: >
  Query and manage a Veda knowledge store: semantic + fulltext search,
  file upload/read/list, structured collections, and SQL. Use when the
  task needs to search the user's knowledge base, fetch a known file, or
  run SQL/aggregations over stored collections.
license: MIT
compatibility: >
  Requires network egress to the Veda server. Reads VEDA_SERVER and
  VEDA_KEY (wk_…) from the environment. Python 3.10+.
metadata:
  author: Joe Kong
  version: "0.1.0"
allowed-tools: Bash Read
---
```

硬规则：`name` 必须 == 目录名 `veda`；`allowed-tools` 是**空格分隔字符串**不是数组；`description` 决定触发，必须写清"做什么 + 何时用"。正文复用现有 `skill.md` 的命令语义/决策表（已更新二进制+PDF 行为），但把"运行 `veda <cmd>`"改成"运行 `python scripts/veda_client.py <cmd>`"。

### 3.3 `scripts/veda_client.py`（骨架）

```python
#!/usr/bin/env python3
# Thin REST wrapper over veda's data plane. Mirrors crates/veda-cli/src/client.rs.
# Auth + endpoint come from the sandbox-injected env (never hard-coded).
import os, sys, json, urllib.request

BASE = os.environ["VEDA_SERVER"].rstrip("/")   # e.g. https://veda.ddmc-inc.com
KEY  = os.environ["VEDA_KEY"]                    # wk_…

def call(method, path, body=None):
    req = urllib.request.Request(
        f"{BASE}{path}", method=method,
        data=json.dumps(body).encode() if body is not None else None,
        headers={"Authorization": f"Bearer {KEY}", "Content-Type": "application/json"})
    with urllib.request.urlopen(req) as r:
        return r.read().decode()

# subcommands: search / grep / ls / cat / sql …  (paths per client.rs)
# e.g.  search:  POST /v1/search  {"query":..., "mode":"hybrid", "limit":10}
```

> 精确的 path/字段以 `crates/veda-cli/src/client.rs` 为准（写脚本时对着抄一遍）。stdlib urllib 可做到**零第三方依赖**，连 `requirements.txt` 都省了，平台依赖分析最干净。

## 4. 凭证与环境变量

- **不写死在脚本里**。在 **Skill 沙箱版本**的「运行环境变量」里配：
  - `VEDA_SERVER=https://veda.ddmc-inc.com`
  - `VEDA_KEY=wk_…`（目标 veda workspace 的 key）
- 沙箱 → `veda.ddmc-inc.com` 的网络出口：Joe 判断**应该是通的**（都是内网 `*.ddmc-inc.com`），上线前实测一把。

## 5. 发布与接入流程

1. 把 §3 的仓库推到 GitLab（先 `内部` 可见性）。
2. Plugin/Skill 市场 →「添加 Plugin/Skill」→ 登记仓库 + branch；等平台**校验/同步**通过（市场列表会显示"已同步/通过"）。
3. Skill 沙箱 →「新建沙箱」→ 选中 `veda` skill → 配运行环境变量（§4）+ CPU/内存最小档 → **发布版本**。
4. 拿沙箱的 **Auth Token**。
5. Agent 侧：用 Auth Token 调沙箱 **Open API** 调用 veda 能力；在「调用监控」看调用量/成功率/日志。

## 6. 比塞沙箱更值得想的两个长期入口

- **MCP 网关**：平台 agent 都是 RAG+MCP 型。给 veda 包一个薄 **MCP server**（封装同一套 REST）接 MCP 网关，比 skill 沙箱更原生——agent 直接当 MCP 工具用，免每个沙箱发版。工作量与 §3 的脚本相当。
- **向量存储 / 知识库后端**：导航里本就有「向量存储」「知识库」两个 tab，而 veda 的定位是"承接公司向量服务"。**veda 的终局更可能是去做这两个 tab 的后端**，skill 沙箱只是让现有 agent 先用上 veda 的过渡。

## 7. 待确认

1. 沙箱运行时是**把每个脚本暴露成可调用工具**，还是**跑一个子 agent 读 SKILL.md**？决定脚本接口怎么设计——照着市场里 `dbpaas-skills`（你们团队的）或 `mysql-assistant` 抄一遍最快。
2. 沙箱 → `veda.ddmc-inc.com` egress 实测放通。
3. 注入哪个 veda workspace 的 `wk_`；多调用方要不要各自独立 key（对应 review backlog 的跨租户 key 问题）。
4. 可见性（内部/公开）。

## 8. 状态与剩余步骤

骨架已生成在 `~/code/personal/veda-skill/`（脚本/JSON/frontmatter 已本地校验通过）。

- ✅ **已推送**：私有仓库 `git.ddxq.mobi/middleware/dbpaas/veda-skill`（push-to-create over SSH，commit 05958eb）。glab token 失效，是靠 SSH push-to-create 建的库。

剩余：

3. **冒烟测一次 REST 链路**（唯一没本地验过的环节，需真 `wk_` + 网络直连绕 Clash）：
   ```sh
   VEDA_SERVER=https://veda.ddmc-inc.com VEDA_KEY=wk_… \
     python3 skills/veda/scripts/veda_client.py ls /
   ```
4. Plugin/Skill 市场登记仓库 → 等校验/同步通过。
5. 新建 Skill 沙箱 → 选 `veda` → 配 `VEDA_SERVER`/`VEDA_KEY` 运行环境变量 → 发布版本 → 拿 Auth Token → agent 接 Open API。

> v0 脚本只支持单文件 `cp`（不做目录递归上传）；够 agent 用，需要再补。
