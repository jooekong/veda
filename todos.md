# Project Todos

## Active
- [ ] 激进重构 outbox 去重：删除 enqueue 期去重三件套（`try_insert_outbox_for_file` / `has_pending_event` / `enqueue_dedup`），改为 worker claim 后内存 coalesce；enqueue 全部裸 insert。详见 `docs/plans/outbox-dedup-refactor.md`
- [ ] db workspace 接业务方前待办：H1 软删资源的 Milvus GC（存储泄漏）、M1 启动校验 Milvus 维度（与 fs W4 合并）、A1 collection 内存天花板 capacity 验证 + lazy load/LRU。详见 `docs/plans/db-workspace-followups.md`

## Completed
