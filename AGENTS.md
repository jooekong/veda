# AGENTS.md — 工作协议

> 这是地图，不是手册。从这里出发，按指针深入。

---

## 文档地图


| 文档                | 职责                      | 何时读     |
| ----------------- | ----------------------- | ------- |
| `ARCHITECTURE.md`       | 系统现状：模块结构、已实现能力、已知问题    | 每次开始工作前 |
| `docs/design/plans.md`  | Phase 总览 + 当前 sprint 任务 | 每次开始工作前 |
| `docs/design/design.md` | 整体设计、API 定义、Schema 定义   | 做设计决策前  |


---

## 完成任务后必须更新


| 事件           | 更新哪里                     |
| ------------ | ------------------------ |
| 实现新功能 / 修改架构 | `ARCHITECTURE.md`        |
| 完成 sprint 任务 | `docs/design/plans.md`（勾选 + 状态） |


**禁止**：把规划中的能力写成已实现。

---

## 技术约定

- Rust Cargo workspace，八个 crate：
  - `veda-types` — 零依赖的领域类型和错误定义
  - `veda-core` — trait 定义 + 业务逻辑（不依赖具体存储实现）
  - `veda-store` — MySQL + Milvus 的 trait 实现
  - `veda-pipeline` — embedding、chunking、文本提取（PDF/OCR planned）
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
- 简化偏好：能 MySQL 不上 Kafka，能 partition 不上 sharding，能单 pod 不上多 pod 并发保护，能 row count 不上 checksum

---

## 测试约定

- 集成 / E2E 测试用测试环境真实 Milvus / MySQL / embedding，避免 mock
- 单元测试可以 mock 纯逻辑（filter parser、PK 拼接、配置加载等）
- 每个 Stage 收尾要有一个跑真实依赖的集成测试作为 DoD
- 测试端点写在 `config/veda.test.toml` 或环境变量，不要 hard-code

---

## 评审约定

- 收到 Codex / subagent / 其他 reviewer 的 findings，**独立评判，不照单全收**
- 分三类讲给 Joe：真问题（必修）/ 边缘（取舍）/ 过度（拒绝）
- 不要默默全部应用补丁，要让 Joe 看到取舍逻辑

---

## 文档约定

- 格式：Markdown
- 路径：`docs/` 目录

