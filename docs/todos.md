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

## veda_dentries 的 collation 未固定（2026-08-03 发现）

`veda_dentries` 的 bootstrap DDL（`crates/veda-store/src/mysql.rs`）没有指定
`CHARACTER SET` / `COLLATE`，`path` / `parent_path` / `name` 三列继承**建库时的
默认值**。测试库上是 `utf8mb4_0900_ai_ci`（大小写 + 重音均不敏感）。

`get_dentry` / `list_dentries` 都是 `WHERE path = ?` / `parent_path = ?` 直接比较
该列，所以**路径查找的大小写敏感性取决于建库默认值，不是代码决定的**：

- 当前测试库上，`/Docs` 与 `/docs` 是同一个目录（第二次 `ensure_parents` 复用第一个），
  但两个文件因 `path_hash = SHA2(path)` 各自存在 —— 目录不敏感、文件敏感的混合语义
- 换一个默认 `utf8mb4_bin` 的库部署，同一份代码行为就不同

影响面比看起来大（FUSE / CLI / 平台面所有按路径定位的操作）。固定它需要：
1. DDL 显式声明 collation
2. 对已有生产表做 `ALTER TABLE ... CONVERT TO` 迁移
3. 想清楚要哪种语义 —— 改成敏感会让现在互相覆盖的路径突然分裂

未排期。发现于 workspace map 的 `file_count` 实现（见
`docs/plans/gitignore-and-workspace-map.md` v4 记录）。
