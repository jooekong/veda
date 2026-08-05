# Agent Token 管理 — 目录感知凭证绑定(定稿设计)

| | |
| --- | --- |
| 版本 | v1.0(替代《AgentPass v0.1》全量设计,评审裁决见附录 A) |
| 日期 | 2026-08-05 |
| 真实消费方 | **veda**(`wk_`/`vk_`,CLI + 原生 MCP)、**AI DB Bridge / adb**(`adb_`,REST + 原生 MCP) |
| 核心场景 | 同一开发者,不同项目目录 → 同一服务的不同 token;多个 Agent 会话并行;对日常使用零侵入 |

> **TL;DR**:不做 PATH shim、不做 Keychain、不做 SQLite、不做 Git 身份归一化、不做 Service SPI。
> 三个小交付物解决全部真实场景:
> **A.** veda CLI 内建目录绑定(约百行 diff,veda 场景当场闭环);
> **B.** `agentpass` 单二进制:一个 0600 TOML 存 token + 最长路径前缀解析 + `env/exec` 注入;
> **C.** `agentpass mcp <service>` stdio→HTTP 代理:Coding Agent 全局注册一次,按启动目录自动带对 token,token 永不进 Agent 进程。

---

## 1. 问题

### 1.1 最小问题陈述(沿用 v0.1,这个表述是对的)

```
~/work/project-a  + 同一个服务  -> token-a
~/work/project-b  + 同一个服务  -> token-b
用户在两个目录中都只执行:  claude / veda …
```

核心能力 = **按进程启动时的目录确定唯一凭证**,而不是让用户手工 export/unset,也不是把 token 写进项目目录。

### 1.2 现状痛点(具体到两个消费方)

- **veda CLI**:凭证优先级 `--key` flag > `$VEDA_KEY` env > `config.toml` 的 `active_workspace`。`active_workspace` 是**全局指针**——在项目 A 里 `veda ws switch` 会改变所有目录、所有并行会话的身份;env 方案则要求每个 shell 手工 export,正是要消灭的操作。
- **veda MCP**:Coding Agent 通过 `.mcp.json` http 条目 + `Bearer wk_...` 接入。按项目区分 workspace 意味着把明文 `wk_` 粘进每个项目的 `.mcp.json`(在仓库目录里,一步之遥就被提交),或者退回 env 注入。
- **adb**:数据面 REST `/v1/call` + MCP `/mcp`(stateless Streamable HTTP),`Bearer adb_...`。身份是 agent 级、key 即身份,不同项目用不同 `adb_` key 的诉求与 veda 完全同构。adb 没有第一方本地 CLI,token 目前只能走 env 或明文配置。

### 1.3 关键洞察(v0.1 遗漏的)

v0.1 把所有消费方假设为**不可修改的第三方二进制**(claude/codex/gemini),于是需要 PATH shim 去"从外面"注入。但真实的两个消费方都有更短的第一方路径:

1. **veda CLI 是自己人**——它自己就知道 cwd,在进程内解析绑定即可,零注入、零 shim。
2. **adb(和 veda)的 Agent 接入走 MCP**——MCP stdio transport 本身就是一个第一方进程边界:Agent 负责在项目目录下拉起我们的进程,cwd 免费送到,token 留在代理进程内。v0.1 的"Native Credential Helper 协议"不需要发明,**MCP 就是那个协议**。

剩下的泛化需求(任意工具 × 任意服务)用显式的 `env/exec` 逃生门覆盖,不值得为它建 shim 体系。

---

## 2. 对 v0.1 的评审总评

**对的部分**(全部保留):问题定义与"目录→身份"最小陈述;最长路径组件前缀匹配(monorepo);默认拒绝、缺绑定即失败不猜测;token 不进 argv/日志/status 输出;并行会话按启动时上下文隔离;无 daemon、local-first;"选择(Resolver)/存储(Provider)/交付(Delivery)解耦"作为**代码内部分层**成立。

**过度设计的部分**:v0.1 是一份"通用 Workspace-aware Credential Manager"的产品设计,规模按周计;而真实需求是两个自家工具的目录级 token 选择,按天计。最重的四处:PATH shim 拦截 Coding Agent(讽刺地成为全设计**最大侵入点**,alias/IDE/版本管理器冲突,v0.1 自己的风险表已承认)、跨平台 OS Keychain 默认存储(Linux Secret Service/headless/弹窗是复杂度黑洞,而 token 最终仍进子进程 env)、SQLite(几十条映射,TOML 可手改可 diff)、Git remote 归一化 + 身份变更 fail-closed(为"重克隆仓库"这种低频事件引入常驻复杂度,重新 bind 一条命令即可)。逐项裁决见附录 A。

另一个定位信号:v0.1 通篇未提 veda——说明它在为假想的泛化场景设计,而不是为手上的消费方设计。

---

## 3. 设计原则

1. **第一方优先**:能改自家消费方,就不在系统层拦截。解析发生在消费方(或其代理)进程内,cwd 是免费输入。
2. **零日常侵入**:用户命令不变(`veda …`、`claude`);新增操作只有一次性的 `add` / `bind`。不改 PATH、不写 shell profile、不要求 direnv。
3. **Fail-closed,不猜测**:该目录没有绑定 → 明确报错 + 可执行的修复提示;绝不静默落到错误身份(veda 保留现有 `active_workspace` 回退,属显式配置的既有语义,不算猜测)。
4. **token 不出现在**:命令行参数(ps 可见)、日志、`status/which` 输出(只显示指纹如 `adb_…7f3a`)、任何仓库内文件。
5. **最简存储**:0600 的 TOML,与 veda `config.toml`、`~/.aws/credentials`、git-credential-store 同一信任级别。不自建密码库。

---

## 4. 方案总览

```
                 ┌────────────────────────────────────────────┐
                 │ A. veda CLI(内建)                          │
  veda …  ──────▶│ config.toml: [workspaces] + [bindings]     │──▶ veda-server
                 │ flag > env > 目录绑定 > active_workspace    │    Bearer wk_
                 └────────────────────────────────────────────┘
                 ┌────────────────────────────────────────────┐
  claude/codex   │ C. agentpass mcp <service>(stdio 代理)     │──▶ hub /mcp   Bearer adb_
  (MCP stdio, ──▶│    按代理进程 cwd 解析 → 注 Bearer 转发      │──▶ veda /mcp  Bearer wk_ (P1)
   全局注册一次)  │ B. agentpass store:~/.config/agentpass/     │
                 │    config.toml(0600):services/credentials/  │
  任意工具 ──────▶│    bindings;env / exec 显式注入(逃生门)     │──▶ REST 脚本、demo 等
                 └────────────────────────────────────────────┘
```

- 每个工具自持凭证:veda 的 `wk_` 留在 veda `config.toml`(现状),adb 及未来无第一方 CLI 的服务进 agentpass store。**共享的是解析约定**(规范化物理路径 + 最长组件前缀 + fail-closed),不强行合库。
- P0 **hub / veda-server 零改动**,全部是客户端侧。

---

## 5. 交付物 A:veda CLI 内建目录绑定

### 5.1 配置形态

`~/.config/veda/config.toml` 新增一节(值 = 既有 `[workspaces.<alias>]` 的别名,**不引入新秘密**):

```toml
[bindings]
"/Users/joe/work/project-a" = "project-a"
"/Users/joe/work/platform/services/order" = "order"
```

### 5.2 命令与语义

- `veda bind <alias>`:在 Git 仓库内默认绑定到 **repo root**(`git rev-parse --show-toplevel`,仅取路径,不读 remote);`--here` 绑当前目录(monorepo 子目录);`--path <p>` 显式路径。alias 必须已存在于 `[workspaces]`,否则报错并提示 `veda workspace add`。
- `veda unbind [--path <p>]`:删除绑定。
- 凭证优先级:**flag > env(`$VEDA_KEY`)> 目录绑定(新)> `active_workspace`**。env 仍然压过绑定——保持"显式的进程级意图必须获胜"的既有语义,CI/脚本不受影响;但当 env 覆盖了一条本可命中的绑定时,`veda status` 明确提示(遗忘在 shell profile 里的全局 export 是本系统头号 footgun,要亮出来)。
- 匹配规则:cwd 取物理路径(解析符号链接)后做**最长路径组件前缀**匹配——`services/order` 命中 `services/order/src`,不命中 `services/order-old`;多条命中取组件数最多者。同 path 键唯一(map),重复 bind 即覆盖(需确认),**歧义被构造性消除**,无需冲突仲裁机制。
- `veda status` 的 key 来源标注从现有的 flag/env/config 扩展出 `binding(<path>)`;dangling alias(绑定指向已删 profile)报错口径与现有 dangling `active_workspace` 一致。

### 5.3 边界

- `veda ws switch` / `active_workspace` 语义不变,仅作为无绑定时的回退——存量用户零感知。
- 绑定只选 workspace,不选 server(`server_url` 仍全局;多 server 需求出现时再在 `WorkspaceEntry` 上加可选字段,先不设计)。
- veda-fuse 不参与:mount 本身就是显式的长期绑定(`--workspace` / env),没有"按 cwd 自动选"的问题。

---

## 6. 交付物 B:`agentpass` 最小凭证工具

单二进制(Rust)。服务于 adb 及一切没有第一方 CLI 的服务;veda 的 token **不**迁入。

### 6.1 存储

`~/.config/agentpass/config.toml`,0600,创建即自动(无 `init` 命令):

```toml
[services.adb]
env = "ADB_TOKEN"                              # env/exec 注入的变量名
mcp_url = "https://dbbridge.company.com/mcp"   # mcp 代理上游(可选)

[credentials.adb]
team-a = "adb_xxxx"          # 值先为裸 token 字符串;
team-b = "adb_yyyy"          # 未来 provider 化 = 值升级为 inline table(向前兼容,P2)

[bindings.adb]
"/Users/joe/work/project-a" = "team-a"
"/Users/joe/work/project-b" = "team-b"
"*" = "team-default"         # 可选全局默认,仅 bind --global 显式创建
```

known-service 模板内置 `adb` / `veda` 的 env 变量名(`ADB_TOKEN`/`VEDA_KEY`),URL 一律用户提供;其余服务首次 `add` 时问一次 env 名。这就是 v0.1 "Service Adapter" 的全部残余:**配置表一行,不是 trait**。

### 6.2 解析规则(与 veda 同一约定)

对 `(service, cwd)`:

1. 服务对应 env 变量已显式设置 → 用之(source=env;若同时存在被遮蔽的绑定,stderr 提示);
2. `[bindings.<service>]` 最长路径组件前缀命中 → 该 credential(名字 dangling → 报错);
3. `"*"` 全局默认(若有);
4. 都没有 → **fail-closed**:`no credential bound for adb under /path — run: agentpass bind adb <name>`。

### 6.3 CLI 面(全集,刻意小)

| 命令 | 说明 |
| --- | --- |
| `agentpass add <service> [<name>]` | 录入 token:无回显 TTY 或 `--stdin`;**禁止**经命令行参数传 token |
| `agentpass rm <service> <name>` | 删除 credential(连带提示受影响绑定) |
| `agentpass bind <service> <name>` | 同 veda:默认 repo root,`--here` / `--path` / `--global` |
| `agentpass unbind <service> [--path]` | 解绑 |
| `agentpass which <service>` | 显示命中的 credential 名 + 指纹 + 命中路径 + 来源;不显示 token |
| `agentpass status` | cwd 视角的全服务解析结果 + 文件权限自检 + dangling 绑定 + env 遮蔽提醒(v0.1 的 doctor 折叠于此) |
| `agentpass env [<service>…]` | 输出 `export` 行供 `eval`(显式逃生门;可配合 direnv,但不默认推荐——整 shell 暴露正是 v0.1 正确反对的) |
| `agentpass exec [-s <service>]… -- <cmd>…` | 解析→注入 env→`exec`;省略 `-s` = 注入 cwd 下所有可解析服务,注入了什么(仅名字)打到 stderr |
| `agentpass mcp <service>` | 见交付物 C |

无 `init` / `doctor` / `migrate` / `request-bind` / OAuth / 过期刷新。

---

## 7. 交付物 C:MCP stdio 代理(`agentpass mcp`)

### 7.1 机制

Coding Agent 把它当普通 stdio MCP server 拉起;它按**自身启动时的 cwd**(= Agent 的项目目录)走 §6.2 解析出 token,然后在 stdio JSON-RPC 与上游 Streamable HTTP `/mcp` 之间逐消息转发,注入 `Authorization: Bearer <token>`。

- 两个真实上游(hub `/mcp`、veda `/mcp`)都是 **stateless** Streamable HTTP → 转发 = 每消息一次 POST:响应 `application/json` 直传;`text/event-stream` 取其中 response 消息;notification(无 id)fire-and-forget。首版不支持 server→client 反向请求(两家上游都不用)。等价于社区 `mcp-remote` + 一次本地 token 查找,估计数百行。
- token 只存在于代理进程与 HTTPS 请求头中,**不进 Agent 进程 env、不进任何 Agent 配置文件**——v0.1 的"保护模式/Native Helper"以零新协议达成。
- 会话不变量(沿用 v0.1,正确):解析发生在代理启动时;Agent 会话中途 `cd` 不换身份。并行会话各自拉起代理进程,天然隔离。
- 解析失败时以 MCP 协议内错误暴露修复提示(工具列表为空 + 说明,或 initialize 后首个 tools/list 返回指导性错误),不静默。

### 7.2 接入(一次性,全局)

```bash
# Claude Code(用户级,一次注册,所有项目按目录自动选 token)
claude mcp add --scope user adb -- agentpass mcp adb
# Codex:~/.codex/config.toml [mcp_servers.adb] command/args 同形
# Gemini CLI:~/.gemini/settings.json mcpServers 同形
```

也可按项目提交 `.mcp.json`(内容只有命令,无任何秘密):

```json
{ "mcpServers": { "adb": { "command": "agentpass", "args": ["mcp", "adb"] } } }
```

### 7.3 veda 的对应物(P1)

`veda mcp` 子命令:同样的 stdio 代理,读 veda 自己的 `[bindings]` + `[workspaces]`,转发到 veda-server `/mcp`。项目级知识库从"明文 `wk_` 粘进 `.mcp.json`"升级为零秘密配置;现有 http + 用户级全局 key 的接法(公司知识库跟人走)保持不变。若 agentpass 落在 veda workspace(见 §11),两者共享同一个解析 crate。

---

## 8. 安全边界(诚实清单)

**防**:token 进入仓库/被提交;跨项目误用身份(错误 token 打到服务);token 出现在 ps/日志/状态输出;并行会话串身份;(MCP 代理模式)prompt injection 让 Agent 读出 token——token 不在 Agent 进程内。

**不防**(与 v0.1 非目标一致,明说):同一 OS 用户下的其他进程读 0600 文件或调用 `agentpass mcp`(本机同用户即信任边界,与 `~/.aws/credentials`、veda `config.toml` 现状同级);`env/exec` 逃生门模式下目标进程自然可见被注入的变量;用户显式 env override 指错身份属用户决策。

**存储立场**:先文件后 Keychain。理由:token 无论存哪最终都要进子进程 env 或请求头,Keychain 只缩小"静态落盘"一个面,却带来 Linux Secret Service 依赖、headless/SSH 失效、GUI 弹窗打断"零侵入"三项成本。P2 把 credential 值升级为 `{ keychain = true }` 形态即可无损引入,现在不做。

---

## 9. 分期

### P0(核心闭环,估计个位数人日)

| 项 | 内容 |
| --- | --- |
| veda 目录绑定 | `[bindings]` + `bind/unbind` + 优先级插入 + `status` 来源标注(§5) |
| agentpass 核心 | store + `add/rm/bind/unbind/which/status/env/exec`(§6) |
| adb MCP 代理 | `agentpass mcp adb`(§7),hub 零改动 |
| 文档 | veda skill.md 与 adb 接入文档补目录绑定路径 |

### P1

`veda mcp` 子命令;`agentpass mcp veda`(如决定不做 `veda mcp` 时的替代);Cursor 等 GUI 宿主的接入指引(见 §11 风险);Linux 打包。

### P2(出现真实需求再动)

Keychain provider;credential 过期提醒;多 server 的 veda 绑定;团队级声明文件。

### 明确不做(v0.1 的以下部分整体裁掉)

PATH shim 及 `init` 装 shim/`doctor` 查 shim;OS Keychain 默认存储;SQLite;Git remote 归一化 / anchor remote / `RepositoryIdentityChanged` / submodule·worktree 特判;Service Adapter trait(discover/inject SPI);Native Credential Helper 自定义协议与 `workspace_id`;File-backed Provider(dotenv/JSON Pointer/YAML/TOML 定位)及 `migrate` 机械;`.agentpass.yaml` 声明 + 审批流;`request-bind` UI;agent_selector;OAuth/命令式 provider/过期刷新;GitHub/OpenAI/Anthropic adapter;daemon/RBAC/中心同步(v0.1 也不做,维持)。

---

## 10. P0 验收标准

1. 项目 A/B 两个终端并行:`veda ls /` 各自命中 workspace A/B;`active_workspace` 全程未被改动;互不影响。
2. 同两目录并行两个 `claude` 会话,adb MCP 工具调用分别携带 token A/B;两个项目目录内**不存在任何含 token 的文件**;`ps` 全程看不到 token。
3. 无绑定目录:veda 回退 `active_workspace`(现状不回归);`agentpass mcp/which` 给出含 `bind` 命令的明确错误,不猜测。
4. `$VEDA_KEY` / `$ADB_TOKEN` 显式设置时覆盖绑定;`veda status` / `agentpass status` 标注 env 来源并提示被遮蔽的绑定。
5. `which/status/日志` 只出现指纹;两个 config 文件权限 0600;dangling alias/credential 报错含可执行修复提示。
6. monorepo:`services/order` 与 `services/order-old` 不互相误命中;子目录绑定压过 repo root 绑定。
7. 卸载 = 删二进制 + 删配置文件;无 PATH/shell profile 残留;veda 侧删除 `[bindings]` 节即完全回到现状。

---

## 11. 风险与开放问题

| 风险 | 应对 |
| --- | --- |
| MCP 宿主拉起 stdio server 的 cwd 不是项目目录(终端 CLI 类可靠继承;**GUI 类如 Cursor 不保证**) | P0 只承诺终端启动的 claude/codex/gemini;代理提供 `--cwd <abs>` 兜底,GUI 场景用项目级配置显式传;`status` 可打印代理视角 cwd 辅助诊断 |
| 仓库移动/重克隆后绑定失效 | 接受:`status` 列出路径已不存在的 dangling 绑定,重新 `bind` 一条命令解决(这正是砍掉 Git 身份机制的代价,划算) |
| 遗忘的全局 env export 遮蔽绑定 | env 仍优先(保 CI/显式意图),但 `status/which/代理 stderr` 主动提醒 |
| binding 文件跨机器同步 | 路径本身 machine-specific,此文件**不设计为可同步**,文档写明 |

**开放问题(需 Joe 拍板)**:
1. agentpass 落地位置:veda workspace 新增 crate(推荐:与 `veda mcp` 共享解析 crate,复用发布链)vs 独立 repo。
2. 命令名:`agentpass` 沿用 vs 更短(`ap`)。
3. `veda mcp` 进 P0 还是 P1(推荐 P1:现有 http 全局 key 接法可用,不阻塞)。
4. adb 是否允许 `--global` 默认绑定(推荐允许——本机开发 token、读优先场景,少一次挫败;`which` 标注 source=global)。

---

## 附录 A:v0.1 逐项裁决表

| v0.1 内容 | 裁决 | 理由 |
| --- | --- | --- |
| 问题定义、"目录→身份"最小陈述 | ✅ 保留 | 准确,是整份文档最有价值的部分 |
| 最长路径组件前缀匹配(monorepo) | ✅ 保留 | 低成本覆盖真实场景,`order`/`order-old` 语义正确 |
| Fail-closed、无绑定不猜测 | ✅ 保留 | 简化后歧义被构造性消除(同 service+path 键唯一),只剩"缺绑定"一种失败 |
| token 不进 argv/日志/status;指纹显示 | ✅ 保留 | 零成本高收益 |
| 并行会话按启动上下文隔离、会话不变量 | ✅ 保留 | 解析在进程启动时发生,天然成立 |
| Resolver/Provider/Delivery 三层解耦 | 🔄 降级 | 作为代码内部分层保留;不做 SPI/trait 面、不做 credential_ref 间接层 |
| PATH Shim 拦截 claude/codex/gemini | ❌ 砍 | 全设计最大侵入点与最脆环节(alias/IDE/版本管理器,v0.1 风险表自认);两个真实消费方各有第一方零侵入路径,shim 只服务假想的泛化场景 |
| OS Keychain 默认 + 三平台矩阵 | ⏬ P2 可选 | 复杂度黑洞;token 终归进子进程;0600 文件与 veda/aws 现状同信任级;文件格式已预留升级位 |
| SQLite | ❌ 砍 | 几十条映射;TOML 可手改、可 diff、可备份 |
| Git remote 归一化 / anchor remote / IdentityChanged / worktree·submodule | ❌ 砍 | 场景键是"目录";bind 时取 repo root 已覆盖便利性;重克隆=重 bind 一条命令,不为低频事件养常驻机制 |
| Service Adapter trait(canonicalize/discover/inject) | ❌ 砍 | 服务=配置一行(env 名 + mcp_url);两个已知消费方,无 N 服务泛化需求 |
| Native Credential Helper 自定义协议 + workspace_id | 🔄 合并 | 目标(token 不进 Agent 进程)保留,载体换成 MCP stdio 代理——协议就是 MCP,零新协议 |
| File-backed Provider(dotenv/JSON Pointer/YAML/TOML)+ migrate 流程 | ❌ 砍 | 一次性 `add` 粘贴即导入;store 本身就是唯一受管文件 |
| `.agentpass.yaml` 声明式配置 + allow/审批 | ⏬ 推迟 | 团队特性;单人两工具阶段零收益 |
| request-bind UI / agent_selector / OAuth / 过期刷新 / command provider | ⏬ 推迟 | 均无当前触发场景 |
| V0.1 服务范围含 GitHub/OpenAI/Anthropic adapter | ❌ 砍 | 目标是 veda+adb;未来服务由通用 `[services.*]` + env/exec 覆盖 |
| 环境变量"先清后注" | 🔄 简化 | `exec` 注入即覆盖同名变量;MCP 代理模式根本不向 Agent 注入 |
| 无 daemon、local-first、开源参考选型 | ✅ 保留 | 与本设计一致(mcp-remote/envchain/aws-vault 的参考仍适用) |
