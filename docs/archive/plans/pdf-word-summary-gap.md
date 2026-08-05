# PDF / Word 文件没有摘要（L0/L1）— 缺陷分析与修复方案

> 状态：**已上线**（2026-08-04 当天发现、修复、部署三节点并完成存量重刷；§7 DoD 全过，§2 的原始复现已闭环——c1c542da 那份 PDF 返回真实 L0/L1）
> 影响版本：0.1.20（引入 PDF/Word 文本提取）起至今 0.1.25，全部节点
> 定性：功能缺失（摘要链路从未接上），非偶发故障、非数据损坏
>
> 实现 commit：
> - `792b504` P0-1/2/3 三道闸（`worker.rs`）+ 5 条闸门单测
> - `c731f5c` 集成测试（真实 MySQL/Milvus + 真 LLM 两条腿）
> - `6723cbf` P1 图片/二进制回 415（可单独摘除，连同其文档 commit）
> - `2f5a14a` §6 存量重刷脚本 `scripts/backfill-blob-summaries.sql`
>
> **尚未执行**：三节点部署 + §7 的 live DoD + §6 存量重刷。
> 实现与本方案的偏差见文末「9. 实现偏差」。

---

## 1. 结论

**PDF / Word 文件永远不会生成 L0/L1 摘要**，`veda abstract` / `veda overview`
对它们会无限期返回 `202 Summary not ready yet`。这不是"还没算完"，是
**摘要任务从来没有被创建过**——等多久都不会有。

其余能力正常：文本提取成功、向量索引成功，所以 `veda cat` 能出文本、
`veda search` 能命中。缺的只有摘要这一条腿。

---

## 2. 复现

生产 workspace `c1c542da`，2026-08-04 07:05:26Z 上传一个 2.2MB PDF：

```sh
$ veda ls / --json
{"mime_type":"application/pdf","name":"FILESYSTEM-BASED MEMORY FOR LLM AGENTS-...pdf",
 "size_bytes":2232154,"created_at":"2026-08-04T07:05:26Z", ...}

$ veda abstract "/FILESYSTEM-BASED MEMORY FOR LLM AGENTS-....pdf"
Summary not ready yet (summary pending). Retry in a few seconds.   # exit 2，20 分钟后仍然如此

$ veda overview "..."          # 同样 pending
$ veda cat "..." --head 15     # ✅ 正常输出提取的正文
$ veda search "filesystem memory for LLM agents"   # ✅ 命中 3 个 chunk
$ veda status --index
indexing: 0 pending, 0 processing, 0 dead          # 队列干净——因为压根没排队
```

`0 pending / 0 processing / 0 dead` 与"任务从未创建"完全自洽，
不要误读成"worker 卡住"或"任务失败"。

---

## 3. 根因：四道闸，任何一道单独修都不够

### 闸 1（根因）：入队时就不产生 SummarySync

`crates/veda-core/src/service/fs.rs:1691` `enqueue_index_outbox()`：

```rust
match source_type {
    SourceType::Text => {
        // ChunkSync + SummarySync
    }
    SourceType::Pdf | SourceType::Word => {
        // 只有 ExtractSync —— 没有 SummarySync
        let outbox = make_outbox(workspace_id, OutboxEventType::ExtractSync, file_id);
        tx.try_insert_outbox_for_file(&outbox, file_id).await?;
    }
    SourceType::Image | SourceType::Binary => {}
}
```

文档注释（`fs.rs:1687`）明确写了 Pdf/Word 只做 "extract 文本然后 embed"，
摘要不在设计内。**这是缺陷的源头。**

### 闸 2：ExtractSync 完成后不接力

`crates/veda-server/src/worker.rs:424` `handle_extract_sync()`：
提取成功 → `upsert_file_extract` → `embed_and_watermark` → 结束。
全程没有 enqueue SummarySync。即使补上闸 1，也建议改这里（见 §5）。

### 闸 3：SummarySync 撞 Blob 直接跳过

`crates/veda-server/src/worker.rs:530`：

```rust
// Blob files (images/binaries) have no text layer to summarize.
if file.storage_type == StorageType::Blob {
    warn!(workspace_id, file_id, "summary_sync skipped: blob file has no text layer");
    return Ok(());
}
```

这是 2026-07 空摘要事故的加固（`4b8edf2` 第 ② 项，修掉了生产 315 条
dead-letter），本身正确，但**注释里的前提对 PDF/Word 不成立**：它们
storage_type 确实是 `Blob`，可文本层在 `veda_file_extracts` 表里。
所以哪怕手工往 outbox 插一条 SummarySync，也会被这里静默跳过。

### 闸 4：load_full_content 对 Blob 直接报错

`crates/veda-server/src/worker.rs:273`：

```rust
StorageType::Blob => Err(VedaError::InvalidInput("binary blob has no text content".into())),
```

即便绕过闸 3，worker 取正文时也拿不到 extract。
对照读路径 `crates/veda-core/src/service/fs.rs:554` 是查 `get_file_extract()`
的——所以 `veda cat` 有文本而 worker 没有，两条路径不一致。

### 为什么表现成"永远 pending"而不是明确报错

`crates/veda-server/src/routes/search.rs:109` `serve_abstract()`：
summary 行不存在 → `summary_pending_response()` → 配了 `[llm]` 就回 202
"生成中"。它无法区分"正在生成"和"永远不会生成"，于是用户被这句话
一直钓着等。

---

## 4. 影响面

- **全部 PDF / Word 文件**（三节点：测试 .161 / .89，生产 .85），
  自 0.1.20 起上传的都没有 L0/L1。
- **连带：目录摘要不完整**。`DirSummarySync` 由子文件 SummarySync 跑完时
  触发（`worker.rs:582`），PDF 没有 SummarySync ⇒ 含 PDF 的目录，其聚合
  摘要从来没把 PDF 算进去，`veda layout` 的目录介绍同样缺这部分内容。
- 检索不受影响（chunk 向量正常），但
  `veda search --detail-level abstract` 对 PDF 命中会拿到空摘要。
- 无数据损坏，无需回滚；修复是纯增量。

---

## 5. 修复方案

### P0-1　ExtractSync 成功后入队 SummarySync

**改 `crates/veda-server/src/worker.rs`，位置在 `upsert_file_extract` 之后
（现 498 行），不要放在函数末尾。**

理由：`handle_extract_sync` 有两条 return 路径——`embed_current` 时刷新完
extract 就 early return（现 500-503 行），末尾才是正常路径。放在
`upsert_file_extract` 之后两条都覆盖，且此时 extract 已落库，SummarySync
一定读得到文本，不需要赌调度顺序。

```rust
self.meta.upsert_file_extract(&extract).await?;

// PDF/Word have a text layer only after extraction, so their SummarySync
// is enqueued here rather than at write time (see enqueue_index_outbox).
let inserted = enqueue_dedup(
    &*self.task_queue,
    workspace_id,
    OutboxEventType::SummarySync,
    "file_id",
    file_id,
    serde_json::json!({ "file_id": file_id }),
    Utc::now(),
).await?;
if !inserted {
    info!(file_id, "summary_sync already pending, skipping enqueue");
}
```

`enqueue_dedup` 已在 worker.rs 顶部 import；写法照抄
`enqueue_dir_summary_sync`（`worker.rs:590`）。不需要 debounce 阶梯——
`has_pending_event` 已经挡住重复入队；如果想对齐现有风格，可复用
`SUMMARY_BURST_WINDOW_SECS` 判断逻辑，但非必需。

> 备选：改闸 1（`fs.rs:1704` 的 Pdf|Word 分支同时入队 SummarySync）。
> **不推荐单独用这条**——写入时 extract 还不存在，SummarySync 可能先于
> ExtractSync 被 claim，读不到文本就空转一轮。要用也得配合 P0-2/P0-3
> 的兜底，收益不如直接改 worker。

### P0-2　闸 3 的跳过条件收紧

`worker.rs:530`。改成「blob **且没有可用 extract**」才跳过：

```rust
if file.storage_type == StorageType::Blob {
    let has_fresh_extract = self.meta.get_file_extract(file_id).await?
        .is_some_and(|ex| ex.source_sha256 == file.checksum_sha256);
    if !has_fresh_extract {
        warn!(workspace_id, file_id, "summary_sync skipped: blob has no extracted text");
        return Ok(());
    }
}
```

**必须校验 `source_sha256 == file.checksum_sha256`**：stale extract 对应的是
旧内容，拿它生成摘要会产出与当前文件对不上的描述。这个新鲜度判定
与 `handle_extract_sync:441` 的 `extract_fresh` 是同一套语义，保持一致。

改完仍然保住 7 月加固的原意：图片 / jar 这类真二进制没有 extract，
照旧跳过，不会重现 315 条 dead-letter。

### P0-3　闸 4 的 Blob 分支读 extract

`worker.rs:273` `load_full_content()`：

```rust
StorageType::Blob => match self.meta.get_file_extract(&file.id).await? {
    Some(ex) if ex.source_sha256 == file.checksum_sha256 => Ok(ex.content),
    _ => Err(VedaError::InvalidInput("binary blob has no text content".into())),
},
```

与读路径 `fs.rs:554` 行为对齐。注意 `handle_chunk_sync`（`worker.rs:300`）
的 blob 跳过**不要动**——PDF 的向量归 ExtractSync 管，那里的注释已经写明。

### P1（可选）　让"永远不会有"和"正在生成"可区分

`routes/search.rs:109`。对 `source_type` 为 `Image`/`Binary` 的文件，
`abstract`/`overview` 返回 4xx + 明确文案（"此文件类型不生成摘要"），
而不是 202。否则用户对着一张 PNG 也会被"retry in a few seconds"钓住。
不阻塞 P0，可单独提。

---

## 6. 存量重刷

修复上线**之后**执行，给已有 PDF/Word 补摘要。建议新增
`scripts/backfill-blob-summaries.sql`，形态照抄
`scripts/refresh-dir-summaries.sql`，**必须带全局限速阶梯**：

- 每个文件 = 2 次 LLM 调用（L0+L1）+ 1 次 embedding，且每个文件跑完还会
  触发父目录 DirSummarySync（又 2 次 LLM），实际放大约 3-4 倍，估算配额时算进去。
- 限速排名用**全局** `ROW_NUMBER()`，不要 `PARTITION BY workspace_id`——
  08-04 那次的教训：分区把 20/min 变成 20/min/workspace。
- `NOT EXISTS` 守卫复刻 `enqueue_dedup`（`veda_outbox` 没有唯一索引，
  裸 SQL 绕过了 Rust 侧 dedup）。
- 时间一律 `UTC_TIMESTAMP()`，`NOW()` 在非 UTC session 会错。
- SummarySync 的 payload 是 `JSON_OBJECT('file_id', f.id)`
  （见 `fs.rs:1894` `make_outbox`）。

筛选条件（对齐 §5 的新鲜度语义）：

```sql
FROM veda_files f
JOIN veda_file_extracts fe
  ON fe.file_id = f.id AND fe.source_sha256 = f.checksum_sha256
LEFT JOIN veda_summaries s ON s.file_id = f.id
WHERE f.source_type IN ('pdf', 'word')
  AND f.storage_type = 'blob'
  AND s.file_id IS NULL
```

先跑 dry-run 数一遍 `COUNT(*)`，把 `4 × count` 对一下上游配额再开闸。
生产节点 .85 无 mysql client，MySQL 操作走 Mac SSH 隧道。

---

## 7. 验收（DoD）

1. 新上传一个 PDF 和一个 .docx，等 worker 跑完：
   `veda abstract <pdf>` / `veda overview <pdf>` 返回真实内容（非 202）。
2. 上传一张 PNG：`abstract` 仍不生成摘要，且 **outbox 无 dead-letter**
   （验证 7 月加固没被改坏）——
   `SELECT COUNT(*) FROM veda_outbox WHERE event_type='summary_sync' AND status='dead'` 为 0。
3. 把一个已有文本文件覆盖成 PDF：摘要更新为 PDF 内容，不残留旧文本的摘要。
4. 含 PDF 的目录，其 `veda abstract <dir>` / `veda layout` 介绍里体现出 PDF 的内容。
5. 集成测试：按项目约定跑真实 Milvus/MySQL/embedding，
   `--test-threads=1 NO_PROXY='*'`（见 `docs/testing/`）。
6. 存量重刷后：`veda_summaries` 中 PDF/Word 文件的 `l0_abstract = ''` 计数为 0
   （2026-07 空摘要事故的哨兵指标）。

---

## 8. 备注

- 对外文档（`web/public/docs/zh/reference.md`、aidoc 仓库）目前只承诺
  PDF/Word "文本被提取并可检索"，**没有**承诺摘要，所以本次是补齐能力
  而非修复承诺违约。修完记得同步这两处文案。
- 用户可见变更记 `CHANGELOG.md`；`ARCHITECTURE.md` 里 PDF/Word 的能力
  描述也要更新。
- 相关历史：`docs/archive/postmortem-2026-07-empty-abstracts.md`（闸 3 的由来）、
  `scripts/backfill-word-extracts.sql`（同类存量重刷的先例）。

---

## 9. 实现偏差

§5 的三处改动语义上照方案实现，闸 1（`fs.rs`）按方案建议**没有**动。
以下几处写法与方案正文不同：

**① 新鲜度判定抽成了一个函数。** 方案在 §5 的三段代码里各自内联写
`is_some_and(|ex| ex.source_sha256 == file.checksum_sha256)`，并要求
「与 `extract_fresh` 是同一套语义，保持一致」。实现把它提成
`worker.rs::fresh_extract(Option<FileExtract>, &str) -> Option<FileExtract>`，
三处调用共用。理由有两条：一是三份拷贝靠人盯着才能不漂移，一份则不会；
二是这个 crate 没有 `MetadataStore` 的 fake（trait ~100 个方法，veda-core
那份 mock 有 748 行且跨 crate 不可复用），把纯判定拿出来是**唯一**能给闸 3
三态 / 闸 4 两态写单测的办法，否则这两张真值表只能靠集成测试间接覆盖。
行为与方案完全一致。

**② P1 的文案是英文，不是方案里的「此文件类型不生成摘要」。**
同一个函数里相邻的两条消息（`summary pending`、`summary generation is
disabled …`）都是英文，混语言更糟。机器可读的部分是 code
`UNSUPPORTED_FILE_TYPE`，方案要求的可区分性由它承担。状态码选 415：不用
404（文件在，能下载），不用 400（请求没毛病）。

**③ P1 顺带改了 CLI 一行。** 方案只点名 `routes/search.rs`。但 CLI 的
`print_summary_layer` 只认 200/202/501/404，新状态会落进
`_ => "unexpected HTTP 415: {json}"`——第一方客户端把新错误渲染成「意外」，
等于只做了一半。加了一条 415 分支（退出码 4，与 pending=2 / disabled=3
可区分）。这一行随 P1 一起摘除。

**④ 重刷脚本按目录给文件排序。** 方案 §6 只规定了筛选条件与全局
ROW_NUMBER。实现额外用 `MIN(path)` 子查询把同目录的文件排在一起——父目录
`DirSummarySync` 的 dedup + 30s debounce 因此能把 N 个文件合成一次聚合，
这是压 §6 那个 3-4 倍放大最直接的杠杆。用子查询不用 JOIN，避免一个 file
万一有多个 dentry 时扇出成重复的 outbox 行。
`@per_min := 4` 的推导：目录重刷实测 8 dirs/min = 16 次 LLM/min 不撞 TPM
墙，这里每文件按 4 次 LLM 算（2 次自身 + 级联父目录），16 / 4 = 4。比目录
重刷保守，因为级联出的 `dir_summary_sync` 由 worker 用 `UTC_TIMESTAMP()`
入队，**不在阶梯上**，会立刻可 claim。

**⑤ 没有覆盖到的：MCP 的 `overview` 工具。** `routes/mcp.rs::tool_overview`
自己走 `get_summary`，有 pending / disabled 两种话术，没有「此类型不生成
摘要」这一种——对着一张 PNG 调 MCP `overview` 仍然会被告知「retry in a few
seconds」。P1 只按方案改了 REST 面。要不要对齐留给 Joe 定。
