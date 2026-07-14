# Project Todos

## Active
- [ ] veda-fuse 暴露 `--version`：0.1.17 已改 `version.workspace = true`（Cargo 元数据正确），但 clap derive 没挂 `#[command(version)]`，二进制仍不认 `--version/-V` 也不嵌版本串；main.rs 顶层 Cli struct 加一行即可，下次发版带上
- [ ] 激进重构 outbox 去重：删除 enqueue 期去重三件套（`try_insert_outbox_for_file` / `has_pending_event` / `enqueue_dedup`），改为 worker claim 后内存 coalesce；enqueue 全部裸 insert。详见 `docs/plans/outbox-dedup-refactor.md`（注意：A-3 lease fencing 提交后需重对其中行号与 claim 签名）
- [ ] db workspace 接业务方前待办：H1 软删资源的 Milvus GC（存储泄漏）、M1 启动校验 Milvus 维度、A1 collection 内存天花板 capacity 验证 + lazy load/LRU（**.85 已上线，A1 优先级需重估**）；另含 backlog 并入的尾巴（embed_cache metrics、500-pk 边界测试、plan 回写清单）。详见 `docs/plans/db-workspace-followups.md`
- [ ] Java SDK 适配 wk_ 数据面（`apiKey`→workspaceKey、去掉 body workspace_id 注入）+ 补 `write_mode` 说明；e2e 重测后发 0.0.2
- [ ] CI 发布 veda-server 二进制（alpha-plan 唯一未兑现尾巴；目前 server 升级仍需在 box 上源码 build）
- [ ] OTLP trace 二期：开工先读 `docs/archive/plans/observability-otlp-plan.md` §0 协议事实
- [ ] review-2026-05-08 两个低优先级尾巴：`/v1/grep` 无 wall-clock 预算；CJK 语言检测把日/韩文误标 zh-CN（`veda-pipeline/src/summary.rs`）

## Completed
- [x] 删除 `tests/server_test.rs`（整文件 mock-only，对生产零覆盖、会与生产 drift）+ 连带清理死 JWT 脚手架与 `jsonwebtoken` 依赖 — 2026-06-24
