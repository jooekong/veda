# 方案：veda 管理接口对接公司 AI Platform

> 状态：**已实现 + codex(xhigh) review（2026-06-03）**。Joe 敲定：前端直连 / 每团队=独立 account / 统一 `wk_` / 砍 JWT / 鉴权纯 key。
> codex review：1 HIGH（`get_active_dataset_by_name` SELECT 漏 `description` → vectors 数据面 500，**已修** + grep 复验 6 个 SELECT 全对齐）+ 1 LOW（`vectors_http_test` body 残留已删的 `workspace_id`，serde 忽略不阻塞）+ 1 INFO（删 `sub_admin_tokens_scope` 使 admin-token 本地覆盖减弱）。剩余：手动集成测试（真实 Milvus/MySQL）、Java SDK wk_ 适配。
> **追加（2026-06-03）platform 账号模型**：`account` 加 `app_id`（唯一索引），`POST /v1/accounts {name, app_id}` 无 email 建账号、返回并由 platform 保管 `vk_`；email 路径保留给 console/CLI；v0 公开建账号、无 `vk_` 补发。SOP 主线已改为 app_id 路径。lib 单测 259 全绿、全 target 编译过。
> **codex review app_id 增量（2026-06-03）**：修 F1 claim 拒绝 app_id 账号 / F3 混合请求(app_id+email/password)拒绝 / F4 既有 `translate_account_email_conflict` 的 1062 失效 bug（抽 `is_mysql_duplicate` 统一 `number()` 检测，顺带修 claim 并发冲突误返 500）/ F5 app_id 存前 trim。**F2 squatting 不改代码**：`POST /v1/accounts` v0 公开，任意方可抢占 app_id——仅限可信内网，platform 接入文档须明示；上公网前须加 platform 凭据。
> 起因：把 veda console 的管理能力搬上公司 AI platform。
> 关联：`web/src/main.ts`（现有 console）、`docs/archive/vectors-merge-plan.md`（db 设计）、`docs/plans/java-sdk-db-plan.md`（受影响 SDK）。

---

## 1. 终态认证模型

> **account `vk_` = 控制面**：platform 后端持有，建/删 workspace、管 key、管 dataset。不外发、不进业务方手。
> **workspace `wk_` = 数据面**：发给业务方，**fs + db 通用**，scope 到单 workspace、可吊销、分读写。
> **删除 JWT 整套**：`AuthWorkspace` 与数据面只认 `wk_`，满足"所有鉴权只做 key 校验"。

集成形态：platform 前端直连 veda REST（CORS 放行 platform 域名）；account 开户/绑 team 走 platform 后端（敏感，不让浏览器裸调公开的 `POST /v1/accounts`）。

## 2. 范围（platform 暴露的管理能力）

- **workspace**：创建 / 查看(列表) / 删除 —— 端点已齐，零改动
- **key**：创建 / 查看(列表，仅元数据) / 删除(吊销) —— 补 2 个端点
- **不做**：JWT、service token 外发、数据面 web UI（文件浏览器/检索台/SQL 台）

## 3. 改动批次（文件级）

| 批 | 文件 | 改动 |
|---|---|---|
| **1 auth** | `veda-server/src/auth.rs` | 删 JWT 全套；抽 `resolve_ws_key()`；`AuthWorkspace` 仅 `wk_`+`kind==Fs`；新增 `AuthDbWorkspace`(`wk_`+`kind==Db`+`read_only`+`require_write`) |
| **2 vectors** | `veda-server/src/routes/vectors.rs`、`veda-types/src/api.rs` | auth `AuthAccount`→`AuthDbWorkspace`；`workspace_id` 取自 `wk_`，删 `resolve_workspace_id`/`load_db_workspace`/scope；`upsert`/`delete` 加 `require_write`；删 4 个 vectors DTO 的 `workspace_id` 字段 |
| **3 key+jwt** | `veda-server/src/routes/account.rs`、`config.rs`、`state.rs`、`Cargo.toml` | 补 `GET /v1/workspaces/{id}/keys`（只回元数据）+ `DELETE .../keys/{key_id}`；删 `POST .../token` 端点+handler；删 `jwt_secret`+`jsonwebtoken` 依赖 |
| **4 console** | `web/src/main.ts` | 删 JWT 按钮；加 key 列表/删除 UI；db workspace 也显示 key 管理；workspace 创建表单加 description |
| **SDK/docs** | `sdk/java`、`docs/api/*`、`examples/`、`ARCHITECTURE.md`、`CHANGELOG.md` | SDK `apiKey(vk_)`→`workspaceKey(wk_)`、删 `workspaceId`；docs/示例/架构表 auth 列更新 |

## 4. description 属性（横切）

workspace 与 dataset 创建均加 `description: Option<String>`（可选）。落点：
- schema：`ALTER TABLE veda_workspaces ADD COLUMN description TEXT NULL`、`veda_datasets` 同（走现有幂等 ALTER 块，1060 dup swallow）
- `veda-types/src/types.rs`：`Workspace` / `Dataset` 加字段
- DTO：`CreateWorkspaceRequest`、`CreateDatasetRequest` 加字段
- `veda-store/src/mysql.rs`：`row_to_workspace`/`row_to_dataset` + 所有 workspace/dataset 的 INSERT/SELECT
- handler：`account.rs` create_workspace、`datasets.rs` create_dataset 透传
- 修所有 `Workspace { .. }` / `Dataset { .. }` 构造点（mock/测试/anonymous bundle）

## 5. 默认决策（可推翻）

1. **db workspace 不自动 bootstrap `wk_`**——platform 显式调 create key（和 fs 一致）
2. **vectors body 删 `workspace_id`**——`wk_` 已绑定（打破未发布 SDK 契约，已认可）
3. **read-only `wk_`**：可 `search`/`query`，不可 `upsert`/`delete`
4. dataset 管理（`/v1/workspaces/{ws}/datasets`）继续走控制面 `vk_`，不动

## 6. DoD

- `cargo build` 全 workspace 过；`cargo test` 单测全绿
- console `tsc`/build 过
- 集成测试（连内网真实 Milvus/MySQL/embedding）**手动跑**：db workspace 用 `wk_` 跑通 upsert/search/query/delete；read-only `wk_` 拒写；fs `wk_` 回归；key list/delete 生效；JWT 端点已移除
- SDK e2e 改 `wk_` 后手动跑通（发版 gate）

## 7. 风险

- 改了 db 数据面 auth 契约 + vectors DTO + Java SDK（未发布，可打破）
- `vk_` 仍是账号根权限、无能力分级（v0 不做，v1 RBAC）；靠"独立 account 隔离 + wk_ 数据面 + vk_ 不外发"收敛爆炸半径
