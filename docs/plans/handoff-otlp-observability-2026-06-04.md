# Handoff：veda 可观测 OTLP metrics 集成（2026-06-04）

## 焦点

接续实现 **veda 一期 OTLP metrics 桥接**。方案已定稿、自包含、过 Codex xhigh review——照着做即可，不需要回看对话。

## 直接开始（唯一实现依据）

读 **`docs/plans/observability-otlp-plan.md`**：
- **§0 协议事实速查**（自包含，含拉配置/连通验证的实测命令）
- **§5 分步实施**（MVP 路径）
- **§8 Codex 处置**（已并入的修正点）

按 §5 MVP 走：`proto + build.rs` 打通编译 → 转 1 个指标（如 `veda_http_requests_total`）→ 拉 collector 发一次 → 在公司监控平台用 `appname=dbpaas-ai-service` 查到 = 端到端通。**建议单独开 `feat/otlp-metrics` 分支**（别混进 app-id 那个）。

## 五个不踩坑（细节都在方案，这里只点名）

1. **不用 opentelemetry-rust**（三重不兼容）→ 自己 `tonic+prost` 生成**旧 proto**（`InstrumentationLibrary`+`labels`，从 cs-oss 仓库 vendored + pin SHA）。
2. metrics DataPoint **双写** `attributes` + deprecated `labels`。
3. histogram **累积桶必须差分**成独立桶（方案 §3.3，否则 `sum(bucket_counts) > count`、分布全错）。
4. cumulative 的 `start_time_unix_nano` 用 exporter 启动时间；值 MVP 统一 `as_double`。
5. collector 地址**从配置服务拉**（前置已解除，不用等机器装本地 agent）；`.161` 的 `7890` 是 cAdvisor 不是 OTLP，别发那。

机器：`root@10.79.51.161`（dogfood，ssh ControlMaster 已建本会话内）。

## 本 session 其它工作的状态（独立任务，勿混）

| 任务 | 位置 | 状态 |
|---|---|---|
| app-id 自动开通租户（server） | veda 分支 `feat/app-id-auto-provision`：`a904e2d`(feat)+`2916045`(docs) | 已提交、`cargo test --ignored` 实跑绿、**未 push** |
| 数据面文档重写（`vectors.md`/`db-workspace-api.md`→wk_） | 同上 `2916045` commit | 已提交、**未 push** |
| APIDoc 平台管理文档（workspace/dataset/key/overview） | `/Users/konglingqiao/code/dd/APIDoc` 分支 `docs/veda-platform-api`，2 commit | 已提交、**未 push** |
| scoped vk_ 越权签 wk_（既有漏洞） | 记 `docs/plans/db-workspace-followups.md` **S1** | backlog，暂不修（等 vk_ 控制面被 app_id 淘汰） |

> ⚠️ **本 OTLP 方案文档 + 本 handoff + `db-workspace-followups.md` 的 S1，目前都混在 `feat/app-id-auto-provision` 工作区未提交**。开 `feat/otlp-metrics` 前先把可观测相关文件 stash/cherry 过去，或先把 docs 单独提交，别让它们跟 app-id feature 缠在一起。

## Memory（新 session 自动加载，无需手动读）

- `reference_company_observability` — 协议事实（与方案 §0 同源，含 7890=cAdvisor、collector 配置服务下发、连通验证等）
- `project_platform_console_migration` — app-id 迁移背景 + S1

## 建议 skills / subagents

- 实现完找 review：`/codex:rescue`（xhigh effort，Joe 惯例；直接 Bash 提交 `codex-companion.mjs task --background --fresh --effort xhigh` + 守护轮询，**别走 Agent 子代理**——本会话实测它会卡在 starting）
- 探查代码：`Explore` subagent
- 验证：方案 §5 端到端（连真实 collector，从 `.161` 上跑）

## 待 Joe 决策的开放点

- endpoint：配置服务直发远端 collector（已倾向、连通已验证）vs 等机器装本地 agent 转发
- 上述各分支的 push / MR 时机（Joe 习惯 push 前确认）
