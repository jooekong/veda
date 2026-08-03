# Project Todos

## Active
- [ ] answer 超时后 Engine 任务存活 + permit 提前释放（REST `/v1/answer` 与 MCP `ask` 同款，2026-07-22 codex review 发现）：route 层 90s timeout drop future 后，`AnswerService::answer_stream` 里 `tokio::spawn(engine.run(...))` 的任务继续跑完内部 80s 预算，而并发闸 permit 已随 handler 返回释放——workspace 短暂超闸（2→3），窗口约数秒到十几秒。修法=给 AnswerService 加 cancel-safe 接口（drop rx 时 abort engine 或 permit 移进 engine task），两个 surface 一起改，单独 PR
- [ ] ~~激进重构 outbox 去重~~ **已搁置（2026-07-29）**：简化审计结论=无性能证据不动稳定写路径。更简的中间路线备忘在 `docs/plans/outbox-dedup-refactor.md` 头部，重启触发条件见 plan（注意 d94bd20 已删 `lease_owner`，重开需重对行号与 claim 签名）
- [ ] db workspace 接业务方前待办：H1 软删资源的 Milvus GC（存储泄漏）、M1 启动校验 Milvus 维度、A1 collection 内存天花板 capacity 验证 + lazy load/LRU（**.85 已上线，A1 优先级需重估**）；另含 backlog 并入的尾巴（embed_cache metrics、500-pk 边界测试、plan 回写清单）。详见 `docs/plans/db-workspace-followups.md`
- [ ] Java SDK 适配 wk_ 数据面（`apiKey`→workspaceKey、去掉 body workspace_id 注入）+ 补 `write_mode` 说明；e2e 重测后发 0.0.2
- [ ] CI 发布 veda-server 二进制（alpha-plan 唯一未兑现尾巴；目前 server 升级仍需在 box 上源码 build）
- [ ] OTLP trace 二期：开工先读 `docs/archive/plans/observability-otlp-plan.md` §0 协议事实
- [ ] review-2026-05-08 尾巴：`/v1/grep` 无 wall-clock 预算（`veda-core/src/service/fs.rs:966`，目前只有 1000 结果 + 50k dentry 两个上限）

## Completed
- [x] CJK 语言检测把日/韩文误标 zh-CN — 随 4b8edf2 修复（`veda-pipeline/src/summary.rs:139` 加 kana/hangul 占比判定，>25% Han 数即回退 en）
- [x] veda-fuse 暴露 `--version`（clap root 挂 `version`）— 随 0.1.21 发版 2026-07-23
- [x] 删除 `tests/server_test.rs`（整文件 mock-only，对生产零覆盖、会与生产 drift）+ 连带清理死 JWT 脚手架与 `jsonwebtoken` 依赖 — 2026-06-24
