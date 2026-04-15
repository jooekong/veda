# AGENTS.md — 工作协议

> 这是地图，不是手册。从这里出发，按指针深入。

---

## 文档地图

| 文档 | 职责 | 何时读 |
|------|------|--------|
| `ARCHITECTURE.md` | 系统现状：模块结构、已实现能力、已知问题 | 每次开始工作前 |
| `docs/PLANS.md` | Phase 总览 + 当前 sprint 任务 | 每次开始工作前 |
| `docs/design.md` | 整体设计、API 定义、Schema 定义 | 做设计决策前 |

---

## 完成任务后必须更新

| 事件 | 更新哪里 |
|------|----------|
| 实现新功能 / 修改架构 | `ARCHITECTURE.md` |
| 完成 sprint 任务 | `docs/PLANS.md`（勾选 + 状态） |

**禁止**：把规划中的能力写成已实现。

---

## 技术约定

- Rust Cargo workspace，八个 crate：
  - `veda-types` — 零依赖的领域类型和错误定义
  - `veda-core` — trait 定义 + 业务逻辑（不依赖具体存储实现）
  - `veda-store` — MySQL + Milvus 的 trait 实现
  - `veda-pipeline` — embedding、chunking、PDF/OCR 提取
  - `veda-sql` — DataFusion SQL 引擎
  - `veda-server` — Axum HTTP 层（薄壳，只做路由和中间件）
  - `veda-cli` — CLI 客户端（纯 HTTP，不直接连数据库）
  - `veda-fuse` — FUSE 挂载（独立编译，不在默认 workspace members 中）
- 错误处理：lib crate 用 `thiserror`，bin crate 用 `anyhow`
- 远程路径用 `:` 前缀（如 `:/docs/readme.md`）
- 认证体系：Account -> Workspace 两级，API Key + Workspace Token
- 一致性策略：正常最终一致 + 异常 Outbox 自愈，不做分布式强一致

---

## 基础约定

- 使用中文回复
- 称呼用户为 Joe
- 不确定时询问 Joe
- 任务完成后询问是否提交，给出变更摘要和建议 commit message，等待确认后执行，不自动 push

---

## 代码约定

- 变量和注释使用英文
- 保持简洁，避免过度抽象
- 无需向后兼容，可自由打破旧格式

---

## 文档约定

- 格式：Markdown
- 路径：`docs/` 目录
