# veda-tunnel 外部接入服务设计

> 状态：**设计待评审**（2026-07-09）
> 第一期范围：企微智能机器人 + 纯检索 + 管控可见性
> 关联：数据面检索 `POST /v1/search`（`web/public/docs/zh/reference.md`）、admin fail-closed 模式（`platform-admin-api-plan.md`）

---

## 1. 背景与目标

veda 已有 fs / db 两条数据面，但都是「被调用」的姿态（CLI、SDK、平台网关拿 `wk_` 来查）。要让知识库能力直接进入企业微信这类 IM 触点，需要一个**主动对接外部服务的 tunnel 层**。

**目标**：新增独立扩展服务 `veda-tunnel`，把 veda 检索能力接入企微智能机器人，并具备**管控可见性**——随时知道挂了哪些机器人、各自绑哪个 workspace/project、连接与工作状态。

**第一期只做三件事**：企微智能机器人（长连接激活）、纯检索直出、管控面（清单 + 状态 + 基础干预）。

## 2. 非目标（第一期不做）

| 不做 | 归属 |
| --- | --- |
| LLM 生成式问答（检索→拼 prompt→生成） | 二期 |
| 飞书 / 钉钉等其他 IM | 架构预留 adapter，暂不实现 |
| 企微 Webhook 短连接模式 | 已选长连接（免加解密、免公网） |
| bot 配置动态入库 / Web 管理后台 | 配置文件即 source of truth，管控面只读 + reload |
| 文档级 ACL / 富权限 | 沿用 veda workspace 级隔离 |

## 3. 整体架构

`veda-tunnel` 是**独立进程**、veda 数据面的**标准 `wk_` 消费者**，veda-server 一行不改。

```
                        ┌──────────────────────────────────────────────┐
   企业微信              │            veda-tunnel（独立进程）              │
 ┌──────────┐          │  ┌─ bot A task ─┐  每 bot：订阅/心跳/退避重连     │      veda 数据面
 │ 机器人 A   │◄──WSS──►│  │ 收消息→msgid  │                              │   ┌──────────────┐
 │ 机器人 B   │◄──WSS──►│  │ 去重→剥@提问  │──── HTTP + wk_（该 bot）────►│ POST /v1/search│
 └──────────┘          │  │ →检索→拼 md   │◄────── SearchHit[] ─────────│ 对应 workspace │
      ▲                │  │ →流式回复      │                              │   └──────────────┘
      │ 流式回复         │  └──────┬───────┘                              │
      └────────────────│         │ 实时更新状态                           │
                        │  ┌──────▼───────┐   ┌───────────────────────┐  │
                        │  │ Bot Registry  │◄──│ Admin API（admin_token）│◄── 运维/Joe
                        │  │ bot→ws/proj   │   │ GET  /admin/bots        │  │  知道哪些 bot、
                        │  │ 状态/计数      │   │ POST /admin/.../reconnect│  │  绑哪个 ws/proj、
                        │  └───────────────┘   │ POST /admin/reload       │  │  状态如何
                        │                       └───────────────────────┘  │
                        └──────────────────────────────────────────────────┘
```

**数据流**：企微把「@机器人 / 单聊提问」推入长连接 → tunnel 剥掉 `@` 得到 query → 用该 bot 的 `wk_` 打 `POST /v1/search` → 拿 `SearchHit[]` 拼成 markdown（片段 + 出处 path）→ 通过长连接流式回给用户。

## 4. 关键约束与决策

| 决策 | 选择 | 理由 |
| --- | --- | --- |
| 接入模式 | 企微**长连接**（WSS） | 免实现 AES-CBC 加解密、免公网入口；官方推荐 |
| 单连接约束 | 一 bot 一长连接，**新连接踢掉旧连接** | 已查证（原文 + `disconnected_event` 事件佐证）；**故 tunnel 全局单实例**，多实例须选主 |
| 进程形态 | 独立 crate `veda-tunnel`，独立二进制 | veda-server 保持无状态可多副本；tunnel 独立发版/重启；单实例约束隔离在 tunnel |
| 检索调用 | HTTP + `wk_` 打数据面 `/v1/search` | 标准消费者姿态，不进程内直调 core；换节点只改配置 |
| 一 bot 一 key | 每 bot 配一个只读 `wk_` → 绑一个 workspace | 满足「一机器人对应一个 veda key」；天然租户隔离 |
| 检索 DTO | tunnel 自定义 6 字段轻量 struct | veda-types 的 `SearchApiRequest`/`SearchHit` 是 server 端单向 derive，方向相反；契约锚在 HTTP JSON |

## 5. crate 结构与模块

```
crates/veda-tunnel/
  Cargo.toml            # bin crate，复用 workspace deps + 新增 tokio-tungstenite
  src/
    main.rs             # 读配置 → 每个 bot spawn 一个长连接任务 → 起 admin server → 常驻
    config.rs           # veda_base_url / admin / [[wecom.bot]] 列表解析
    registry.rs         # Bot Registry：Arc<DashMap<bot_id, BotStatus>>，各任务实时更新
    veda.rs             # 薄 HTTP 客户端：POST /v1/search（Bearer wk_）→ Vec<Hit>
    admin.rs            # Admin HTTP：/admin/bots、/reconnect、/reload、/healthz
    wecom/
      protocol.rs       # aibot_subscribe / msg_callback / respond_msg / events 的 serde 类型
      conn.rs           # 单 bot：连 wss + 订阅 + 心跳 30s + 退避重连 + disconnected_event
      handler.rs        # 收提问 → 去重 → 剥@ → veda.search → 拼 markdown → 流式回
```

依赖全部复用 workspace 现成（`tokio` / `reqwest` / `serde` / `serde_json` / `toml` / `tracing` / `anyhow` / `moka`——moka 做 msgid 去重的 TTL 缓存），**只新增 `tokio-tungstenite`**（WSS 客户端 + rustls）。

**为多 IM 预留**：`wecom/` 是第一个 adapter；未来 `feishu/`、`dingtalk/` 平级新增，`registry.rs` / `admin.rs` / `veda.rs` 通用，`main.rs` 按配置起不同 tunnel。

## 6. 配置模型（一机器人一 key）

`config/tunnel.toml`：

```toml
veda_base_url = "http://10.79.55.85:3000"   # veda 数据面（内网）

[admin]
listen = "127.0.0.1:9100"    # 管控面监听，默认只绑 localhost（运维走 SSH 隧道）
token  = "..."               # admin_token；未配则所有 admin 端点 fail-closed 拒绝

[[wecom.bot]]
name      = "hr-helper"      # 人类可读名，管控面 + 日志用
bot_id    = "..."            # 企微机器人 id
secret    = "..."            # 长连接专用密钥
veda_key  = "wk_xxx"         # 该 bot 检索用的 workspace 只读 key
workspace = "hr-kb"          # 该 key 绑定的 workspace 标识（管控展示用，见 §10.4）
project   = "hr"             # 可选，业务 project 标注（管控展示用）
mode      = "hybrid"         # 可选，默认 hybrid
limit     = 8                # 可选，默认 8

[[wecom.bot]]
name      = "eng-docs"
bot_id    = "..."
secret    = "..."
veda_key  = "wk_yyy"
workspace = "eng-kb"
```

一个 bot = 一条长连接 = 一个 `wk_` = 一个 workspace，支持多 bot 并存。

## 7. 运行时行为

### 7.1 连接生命周期（每 bot 一个 task）

1. 连 `wss://openws.work.weixin.qq.com`，发 `aibot_subscribe`（`bot_id` + `secret`）
2. 订阅成功 → registry 置 `Subscribed`；每 30s 发 `ping` 保活
3. 断线（网络中断 / 收到 `disconnected_event` 被新连接踢）→ 置 `Reconnecting`，指数退避重连
4. 重连成功回到步骤 2

### 7.2 消息处理流

`aibot_msg_callback`（text）→ 按 `msgid` 去重（moka TTL）→ 剥掉 `@机器人` 前缀得 query → `veda.search()` → 取 top-k 的 `content` + `path` 拼 markdown → `aibot_respond_msg` 流式回。非 text 消息回「暂只支持文字提问」。

### 7.3 五秒超时与流式（关键）

企微要求 **5 秒内响应**，而检索可能更久。用长连接自带的流式：先推一帧 `finish:false`（"正在查阅知识库…"）秒回占位，检索完再推 `finish:true` 给结果，全程须在 **10 分钟**内完成。二期接 LLM 生成后同样靠这个机制吸收生成延迟。

## 8. 企微长连接协议（帧清单，实现前需按官方文档核准）

> 以下字段来自官方文档概览（[长连接](https://developer.work.weixin.qq.com/document/path/101463)），**落 `conn.rs`/`protocol.rs` 前逐帧核准确切结构与错误码**。

**上行（tunnel→企微）**：`aibot_subscribe`（订阅握手）、`ping`（心跳）、`aibot_respond_msg`（回复，含 `stream.id`/`finish`）、`aibot_send_msg`（主动推送，二期用）。

**下行（企微→tunnel）触发场景**：

| 场景 | 帧 / 事件 | 第一期 |
| --- | --- | --- |
| 群聊 **@机器人** | `aibot_msg_callback` | ✅ 核心 |
| 单聊发消息 | `aibot_msg_callback` | ✅ 核心 |
| 当天首次进入单聊 | `aibot_event_callback` / `enter_chat` | 可选欢迎语 |
| 点击模板卡片 | `template_card_event` | ❌ |
| 用户点赞/踩 | `feedback_event` | ❌ |
| 被新连接踢掉 | `disconnected_event` | ✅ 触发重连 |

**群聊必须被 @ 才推**（不 @ 不推），天然省流量、无需自己过滤。消息类型 text/image/voice/file/video/mixed，第一期只处理 text。

## 9. veda 检索对接

**请求**：`POST {veda_base_url}/v1/search`，`Authorization: Bearer {veda_key}`

```json
{ "query": "报销流程", "mode": "hybrid", "limit": 8 }
```

**响应** `{ "success": true, "data": [SearchHit...] }`，每个 hit：`content`（chunk 原文）、`path`（出处）、`score`、`score_type`、`chunk_index`。tunnel 只取 `content` + `path` 拼 markdown。

**错误处理**：检索 5xx / 超时 → 回「知识库暂时不可用，请稍后再试」并计入 `error_count`；`401`（key 失效/被吊销）→ 记 `last_error`、该 bot 标红，仍保持长连接（避免因一次鉴权错误断开企微侧）；空结果 → 回「没找到相关内容」。检索设**客户端超时**（如 10s），避免卡死流式窗口。

## 10. 管控面设计

**目标**：随时知道①挂了哪些机器人；②每个机器人绑哪个 workspace/project；③连接与工作状态；并能做基础干预（重连、重载配置）。不做动态入库 / Web 后台（过度）。

### 10.1 Bot Registry 数据模型

进程内 `Arc<DashMap<bot_id, BotStatus>>`，每个连接任务实时更新自己那条：

| 字段 | 说明 |
| --- | --- |
| `name` / `bot_id` | 配置名 / 企微机器人 id |
| `workspace` / `project` | 该 bot 绑定的知识库范围（配置提供，见 §10.4） |
| `conn_state` | `Connecting` / `Subscribed` / `Reconnecting` / `Down` |
| `connected_since` | 本次连接建立时间 |
| `last_msg_at` | 最近处理提问时间 |
| `msg_count` / `error_count` | 累计处理提问数 / 错误数 |
| `last_error` | 最近错误（截断） |

### 10.2 Admin HTTP API

| 端点 | 作用 |
| --- | --- |
| `GET /admin/bots` | 列全部 BotStatus（回答「哪些 bot、绑哪个 ws/proj、状态如何」） |
| `GET /admin/bots/{bot_id}` | 单个详情 |
| `POST /admin/bots/{bot_id}/reconnect` | 强制重连 |
| `POST /admin/reload` | 重读配置文件，增删/更新 bot（热重载，不重启进程） |
| `GET /healthz` | 存活探针（无鉴权） |

### 10.3 鉴权

`Authorization: Bearer {admin_token}`，**fail-closed**：未配 `[admin].token` 则所有 `/admin/*` 一律 403（复用 veda admin console 的 `VEDA_ADMIN_TOKEN` 模式）。`[admin].listen` 默认 `127.0.0.1`，管控面不暴露公网，运维经 SSH 隧道访问。

### 10.4 workspace/project 元数据来源

`wk_` 是数据面 key，tunnel 无法从中反查出人类可读的 workspace 名，故**配置里显式填 `workspace`（+ 可选 `project`）**作为管控展示元数据——tunnel 不解析 key 绑定关系，只把「配置声明的范围 + 运行状态」一起呈现。

> **已定（2026-07-09）**：`project` 为**纯展示标签**——bot 检索统一走 `/v1/search` + `wk_`（一 bot 一 workspace），`project` 只在管控面标注该 workspace 属于哪条业务线，**不触发平台面 project 检索、不走网关 authz**。与「一机器人一 key」自洽，无额外复杂度。

## 11. 可观测

结构化日志（`tracing`）：连接状态迁移、每次提问的 query/命中数/耗时、错误。可选接公司 OTLP（veda 已有先例）：暴露 `bot_up`、`msg_total`、`search_latency`、`error_total` 等 metric，第一期可先只做日志。

## 12. 部署形态

- systemd 单元跑独立二进制（对齐 veda-server 的 `Type=simple` / `Restart=on-failure`）
- **单实例**：长连接单连接约束决定 tunnel 全局只能一个实例持 bot 连接
- **未来多实例高可用**：选主（MySQL `GET_LOCK` 或一行 leader 表抢锁），仅持锁实例连企微；或配置开关指定 active 实例。滚动升级时「新踢旧」天然故障转移（切换瞬间极短空窗，可接受）
- `veda_base_url` 指向哪个节点（生产 .85 内网 / 测试 .161）由配置决定

## 13. 第一期 DoD

1. `cargo build -p veda-tunnel` 产出独立二进制
2. 填一个真实企微测试机器人 + 一个真实 workspace 的只读 `wk_`
3. 订阅成功、心跳保持、拔网自动重连、被踢（`disconnected_event`）能重连
4. 群 @机器人 / 单聊提问 → 收到基于该 `wk_` 对应 workspace 的检索片段 + 出处 `path`
5. 配两个 bot → 各搜各的 workspace 互不串（验证 per-key 隔离）
6. `GET /admin/bots` 能列出全部 bot 及其 workspace/project/连接状态；无 token 时 fail-closed 403
7. 真实企微 + 真实 veda 跑通（集成测试约定）

## 14. 分期

| 期 | 内容 |
| --- | --- |
| **一期** | 企微长连接 + 纯检索直出 + 管控可见性（本文档） |
| **二期** | LLM 生成式问答：handler 里检索后拼 prompt→调 LLM→流式生成。LLM 来源待定（veda `[llm]` / 公司 airouter） |
| **未来** | 多 IM adapter（飞书/钉钉）；多实例选主；富指标 |

## 15. 前置条件与开放问题

**前置条件（需 Joe / 管理员提供）**：
- 企微后台建测试智能机器人 → `bot_id` + `secret`
- 一个装了测试文件的 fs workspace → 只读 `wk_`
- `veda_base_url`（连哪个节点）、`admin_token`

**开放问题**：
1. ~~`project` 语义~~ → **已定 (a) 纯展示标签**（§10.4）
2. admin 面独立端口（`127.0.0.1:9100`）可以吗，还是要并入别的入口？（暂按独立端口）
3. 二期 LLM 生成用 veda 现有 `[llm]` 还是公司 airouter？（第一期不阻塞）

## 16. 实现进度与偏差（2026-07-09）

**已实现（骨架）**：`crates/veda-tunnel` 全套模块（config / registry / veda / admin / wecom{protocol,conn,handler} / main），`cargo build -p veda-tunnel` + 19 单测 + clippy 全绿。含 `config/tunnel.toml.example`、`scripts/deploy/veda-tunnel.service`。

**真机联调（2026-07-09 通过）**：真实企微机器人订阅成功、30s 心跳稳定保持、`/admin/reload` 热切换 veda_key、fs workspace `wk_` @「如何接入DAL」→检索 8 命中 321ms→流式回片段+出处，端到端跑通。DoD 5（多 bot per-key 隔离）未测（单 bot 联调）。

**相对本设计的偏差（均已落代码，记此备查）**：

1. **`disconnected_event` 归属修正**：核实官方文档后确认它不是独立帧，而是 `aibot_event_callback` 下的 `body.event.eventtype`（与 `enter_chat` 同级）；§8 表格把它平列了。`conn.rs` 按 `eventtype` 分流。
2. **协议信封已核实**：每帧 `{cmd, headers:{req_id}, body:{…}}`，订阅/心跳 ACK 无 `cmd`、带 `errcode`。上行用 `json!` 构造，下行用宽松 `RawFrame`（字段全 optional）+ 按 `cmd` 分派，未知帧降级为忽略而非断连。逐帧精确结构仍以官方文档为准。
3. **admin fail-closed 用 404/401**（非 §10.3 写的 403）：对齐 veda-server admin 现状——未配 token → 404（不暴露端点存在），token 错 → 401。
4. **reload 粗粒度全量重建**：`stop all → 重读 config → reseed registry（原地，保 admin 的 Arc）→ respawn all`，不做单 bot 增量 diff（task 生命周期最简、bug 面最小；符合简化偏好）。另实现 `reconnect`（单 bot 停+重启）。
5. **handler 归入 `wecom/`**：企微特定逻辑（剥 @、拼 md）随 adapter 走，未来 `feishu/` 自带 handler。
6. **依赖**：仅新增 `tokio-tungstenite 0.26`（`rustls-tls-webpki-roots`，与 reqwest 同 TLS 栈）；注册表用 `Arc<RwLock<HashMap>>` 而非 dashmap（bot 数小、更新稀疏）。
7. **rustls CryptoProvider（真机联调才暴露的 bug）**：tokio-tungstenite + rustls 0.23，依赖树同时有 aws-lc-rs 和 ring → 无法自动选 provider，首次 WSS 握手 panic。修复：`main()` 启动装 `rustls::crypto::aws_lc_rs::default_provider().install_default()`（对齐 reqwest）+ Cargo 加 `rustls` dep。单测/编译测不到，只有真实 TLS 握手暴露——正是端到端联调的价值。

## 17. 前端 bot 管理 + MySQL 存储（2026-07-09 追加）

**需求变更**：把 bot 配置从「config 文件即 source of truth」改为「MySQL 存储 + 前端增删改」（推翻 §2 原非目标「配置动态入库/Web 后台」）。

**方案**：
- **存储**：MySQL `veda_tunnel_bots` 表（`store.rs`，sqlx，与 veda 同实例）。tunnel 从「零 DB 依赖」变成有状态服务。`config.toml` 的 `[[wecom.bot]]` 降级为**首次 seed**（空表时导入），此后 DB 为准。
- **admin CRUD**（`admin.rs`）：`POST/PUT/DELETE /admin/bots`，经 control loop 复用 spawn/stop **动态增删连接、不重启**。`GET` 返回配置+状态合并视图，secret 不返回、`veda_key` 脱敏（`wk_b36…0f58`）；编辑时 secret/key 留空=保留（SQL `COALESCE(NULLIF(?,''),col)`）。唯一约束 bot_id(PK)/name。
- **前端**：veda web admin console 加 `#/admin/tunnel` 页（`web/src/admin.ts`），列表+增删改表单，复用 admin token。tunnel 返裸 JSON（非 veda `{success,data}` 信封），故单独 `tunnelApi` 封装。
- **部署**：nginx 反代 `location /tunnel/v1/ { proxy_pass http://127.0.0.1:9100/admin/; }`，前端 fetch `/tunnel/v1/*` 同源；tunnel `[admin].token` 配成 = `VEDA_ADMIN_TOKEN`（一个 token 走两后端）。

**联调（2026-07-09，真实 veda_it MySQL）**：连接→建表→seed→spawn→订阅全通；CRUD 全过（POST 201 / 重复 bot_id 409 / PUT 保留 key / DELETE / 401 fail-closed）；前端 `tsc`+`vite build` 过、`/tunnel/v1` 经 proxy 通。前端真机点击未测（chrome 扩展未连）。
