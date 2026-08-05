# Word 文档支持 — 测试环境手动 E2E SOP

> 适用版本：main `02efeaf`（Word 提取 + `veda_file_extracts` 全文落库），部署于测试节点 .161/.89。
> 预计耗时：15 分钟。每步都有「预期」，不符即停，按文末排障查。

## 0. 前置

- veda CLI 已安装，测试环境 workspace key（`wk_...`）在手。
- **mac 直连测试数据面**：`https://veda.dbpaas.dingdongxiaoqu.com`（不是网关域名），
  终端里清掉 http 代理（`unset http_proxy https_proxy all_proxy`），Clash 走 TUN 模式。
- 准备两个样本文件（内容含一个独特哨兵词，方便断言检索命中是本次上传）：

```bash
cd /tmp
printf 'WORD_SOP_%s\n\n这是用于验证 veda Word 提取的中文段落。\n公司数据库连接池的推荐配置是 maxConnectionAge=290 秒。\n' $(date +%s) > wsrc.txt
textutil -convert docx -output sop_test.docx wsrc.txt
textutil -convert doc  -output sop_test.doc  wsrc.txt
grep -o 'WORD_SOP_[0-9]*' wsrc.txt   # 记下哨兵词，下面用 $SENTINEL 指代
```

> 也可以换成任意真实业务 Word 文件（更接近实战），哨兵词换成文件里一句独特原文。

## 1. 上传（原有能力，应无回归）

```bash
veda cp /tmp/sop_test.docx /sop/sop_test.docx
veda cp /tmp/sop_test.doc  /sop/sop_test.doc
veda ls /sop
```

**预期**：两个文件上传成功；`ls` 显示真实 mime（docx 为
`application/vnd...wordprocessingml.document`；.doc 为 `application/msword`
或 `application/x-ole-storage`——取决于生成器，两者都正常）。

## 2. 等 worker 提取（约 10–30 秒）

worker 轮询 outbox → 提取文本 → 存全文 → embedding 进 Milvus。

## 3. 语义检索命中（新能力①：Word 内容可搜）

```bash
veda search "$SENTINEL"
veda search "数据库连接池的推荐配置"
```

**预期**：两条 query 都命中 `/sop/sop_test.docx` 和 `/sop/sop_test.doc`，
返回片段是**可读的提取文本**（不是乱码）。

## 4. Web 预览（新能力②：预览显示全文）

打开 veda-test web console → 对应 workspace → 文件列表 → 点开 `/sop/sop_test.docx`。

**预期**：预览区显示**提取的正文文本**（此前版本这里是「暂不支持预览该格式（Word 文档）」）。
`.doc` 同样。

## 5. 下载完好（原有能力，应无回归）

```bash
veda cp /sop/sop_test.docx /tmp/back.docx && open /tmp/back.docx
```

**预期**：字节与原件一致，Word/WPS 能正常打开——提取不动原件。

## 6. 企微 bot 问答（新能力③：bot 能读 Word 全文）

在接测试 tunnel 的企微群里问 bot：

> 数据库连接池的推荐配置是多少？

**预期**：bot 给出「maxConnectionAge=290 秒」并带出处引用到 sop_test 文件。
过程里若 bot 调了 `read_file`（admin console → Q&A 日志 → 过程 可见），
不再出现「无法读取:binary file」——这是本次修复的核心链路。

## 7. 覆盖写一致性（新能力④：不 serve 旧版本文本）

```bash
printf 'WORD_SOP_V2_new_content 覆盖后的新内容。\n' > w2.txt
textutil -convert docx -output sop_test.docx w2.txt
veda cp sop_test.docx /sop/sop_test.docx
# 立即搜（提取大概率还没跑完）：
veda search "WORD_SOP_V2_new_content"   # 可能暂时无结果 = 正常
# ~30 秒后再搜：
veda search "WORD_SOP_V2_new_content"   # → 命中
veda search "$SENTINEL"                  # → 不应再命中 docx（旧向量已被覆盖清扫）
```

**预期**：窗口期内预览/问答**要么旧要么「提取中」，绝不把旧文本当新内容**；
收敛后新内容可搜、旧哨兵不再命中该文件。

## 8. 删除干净（新能力⑤：不留脏数据）

```bash
veda rm /sop/sop_test.docx /sop/sop_test.doc
veda search "WORD_SOP_V2_new_content"   # → 无结果
```

管理员可选（测试库 SQL 确认零残留）：

```sql
SELECT COUNT(*) FROM veda_file_extracts fe
LEFT JOIN veda_files f ON f.id = fe.file_id
WHERE f.id IS NULL;   -- 孤儿 extracts，恒应为 0
```

## 9. 存量 backfill（管理员，一次性）

测试库跑 `scripts/backfill-word-extracts.sql`（.161/.89 共库只需跑一次）：
存量 Word（此前按 binary 存的）会补索引；存量 PDF 补全文（不重复 embedding）。
跑完后随便挑一个**老 PDF** 在 web 预览打开——应显示文本全文，即 backfill 生效。

---

## 排障

| 症状 | 先查 |
|---|---|
| search 一直不命中 | `.89`: `journalctl -u veda-server --since "5 min ago" \| grep -iE "extract\|error"`；outbox 状态：`SELECT status,COUNT(*) FROM veda_outbox WHERE event_type='extract_sync' GROUP BY status;`（`dead` 增长 = 提取被打死，看 warn 日志里的 skip 原因） |
| 预览仍显示「暂不支持预览」 | 该文件 extracts 尚未生成（等 worker）或提取失败（加密 doc / Word95 / 非 Word 的 OLE 文件按设计跳过，只存不索引） |
| bot 拿不到全文 | admin Q&A 日志看 `read_file` 工具返回；「提取尚未完成」= worker 没跑完，稍后重试 |
| CLI `cat` 输出乱码 | 旧版 CLI 才会：新 CLI `cat` 默认输出提取文本，`--raw` 才是原始字节；下载用 `veda cp` |
