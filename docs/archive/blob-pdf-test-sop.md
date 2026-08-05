# Blob 存储 + PDF 提取 — 手动测试 SOP

> 验证 2026-06-24 新增的「二进制 blob 存储 + PDF 文本提取」。
> 前置：本机能直连 `config/test.toml` 里的测试 MySQL / Milvus / airouter（已验证可达）。
> 通用 CLI 用法见 `docs/testing/manual-test-sop.md`，本文只覆盖本次改动的验证点。

---

## 0. 准备（约 2 分钟）

```bash
cd /Users/konglingqiao/code/personal/veda
cargo build -p veda-server -p veda-cli
alias veda='./target/debug/veda'

# 起 server（config/test.toml 即 server config 结构；自带后台 worker，2s 轮询 outbox）
# NO_PROXY 让 server 绕 Clash 直连内网 MySQL/Milvus + airouter
NO_PROXY='*' no_proxy='*' ./target/debug/veda-server config/test.toml &
until curl -sf http://localhost:3000/v1/ready >/dev/null 2>&1; do sleep 1; done
echo "server ready"

# 一步 onboarding：建 account + Fs workspace + wk_ key，并写进 ~/.config/veda/config.toml
veda init
veda status        # 确认 active workspace=default + reachable
```

> 若 CLI 连不上 localhost（Clash 怪规则）：`export NO_PROXY=localhost,127.0.0.1`。

**造一个有文字层的测试 PDF**（macOS cupsfilter；扫描版/图片 PDF 没有文字层、搜不到）：

```bash
echo "Veda PDF test. Sentinel keyword: SALAMANDER_42. Topics: arctic terns, vector databases." > /tmp/src.txt
cupsfilter /tmp/src.txt > /tmp/test.pdf 2>/dev/null
file /tmp/test.pdf      # 应为 "PDF document"
```

---

## 1. 二进制无损存取（blob 核心）

```bash
# PDF
veda cp /tmp/test.pdf /docs/paper.pdf            # 期望: Written: revision 1
veda cat /docs/paper.pdf > /tmp/paper-rt.pdf
cmp /tmp/test.pdf /tmp/paper-rt.pdf && echo "PASS: PDF 字节无损"

# 任意真二进制（拿编译出的可执行文件当样本）
veda cp ./target/debug/veda /bin/veda.bin
veda cat /bin/veda.bin > /tmp/veda-rt.bin
cmp ./target/debug/veda /tmp/veda-rt.bin && echo "PASS: binary 字节无损"
```

✅ 期望：两个 `cmp` 都静默成功（无损）。证明二进制进 MySQL `veda_file_blobs` LONGBLOB、原样取回。

---

## 2. PDF 提取 → 可检索（ExtractSync 端到端）

```bash
# 第 1 步已 cp paper.pdf。等 worker 消费 ExtractSync（提取文本层 → embed → Milvus）
sleep 10

veda search "SALAMANDER" --mode semantic --limit 5    # 应命中 /docs/paper.pdf
veda search "arctic terns"                            # 默认 hybrid，也应命中
```

✅ 期望：搜到 `/docs/paper.pdf`。证明 PDF 文本层被抽出并索引，**原 PDF 仍是 blob 可下载**（第 1 步已验无损）。

❌ 搜不到先看「故障排查」。

---

## 3. 图片 / 纯二进制：只存不索引

```bash
# 可执行文件（第 1 步的 /bin/veda.bin）不该被检索
veda search "veda" --limit 10 | grep -q "/bin/veda.bin" \
  && echo "FAIL: 二进制被索引了" || echo "PASS: 二进制只存不索引"
```

✅ 期望：PASS。图片 / jar / exe 进 blob 但不进 Milvus（不发 ChunkSync/ExtractSync）。

---

## 4. 文本路径不回归

```bash
echo "machine learning and neural networks notes" | veda cp - /docs/notes.txt
sleep 5
veda cat /docs/notes.txt                       # 正常显示文本
veda cat /docs/notes.txt --head 1              # 行操作正常
veda search "neural networks"                  # 命中 /docs/notes.txt
```

✅ 期望：文本写/读/行操作/检索全部如常（统一 WriteMeta 改动没破坏文本路径）。

---

## 5. N-1：`cat` 二进制读侧对称

```bash
veda cat /docs/paper.pdf > /tmp/x.pdf && file /tmp/x.pdf   # 期望: PDF document（整文件读=原始字节，无损）
veda cat /docs/paper.pdf --head 5                          # 期望: 报错 "'...' is binary; --range/--head need text"
```

✅ 期望：整文件读无损、行操作对二进制**明确报错而非乱码**（写/读对称）。

---

## 6. 类型互转：text → pdf 清旧索引（B2 修复验证）

```bash
echo "old text content about pink elephants" | veda cp - /docs/morph
sleep 5
veda search "pink elephants"          # 命中 /docs/morph（此时是文本）

veda cp /tmp/test.pdf /docs/morph     # 同路径覆盖成 PDF
sleep 10
veda search "pink elephants"          # 期望: 搜不到旧文本（已清）
veda search "SALAMANDER"              # 期望: 命中（新 PDF 内容已索引）
veda cat /docs/morph > /tmp/morph.pdf && cmp /tmp/test.pdf /tmp/morph.pdf && echo "PASS: morph→pdf 无损"
```

✅ 期望：旧文本向量被清、新 PDF 内容可搜、字节无损。证明 text↔pdf 互转的索引覆盖正确（Codex/Claude 双审的核心点）。

---

## 故障排查

| 现象 | 排查 |
|---|---|
| PDF 搜不到 | ① 等久点 `sleep 15`；② 确认真 PDF（`file x.pdf`）——靠 magic bytes 识别不是扩展名；③ 看 server 日志有无 `extract_sync skipped`（提取失败/扫描版无文字层）；④ 查 outbox（下方） |
| 看 outbox 状态 | `veda sql` 引擎没有 outbox 表，直连 MySQL：`mysql -h 10.78.81.148 -u rw_dbpaas -p veda_it -e "SELECT event_type,status,COUNT(*) FROM veda_outbox GROUP BY 1,2;"`，应见 `extract_sync / completed` |
| 验证 source_type/mime | `mysql ... -e "SELECT path,source_type,storage_type,mime_type FROM veda_files f JOIN veda_dentries d ON d.file_id=f.id WHERE d.path LIKE '/docs/%';"`，PDF 应为 `pdf / blob / application/pdf` |
| 扫描版 PDF 搜不到 | 正常——`pdf-extract` 只取文字层，图片 PDF 提取为空、按「无可索引」跳过 |
| server 起不来 | 检查 MySQL/Milvus 可达（`curl localhost:3000/v1/ready`）；embedding `dimension` 必须和 Milvus collection 一致（test.toml 是 1024） |

## 清理

```bash
kill %1                    # 停 server（含 worker）
# 测试数据在匿名 workspace，可整体删或留着；要清 MySQL/Milvus 见 manual-test-sop.md
```

---

## 验收清单

- [ ] §1 PDF + binary 字节无损（2 个 cmp PASS）
- [ ] §2 PDF 正文可被 search 命中
- [ ] §3 二进制只存不索引
- [ ] §4 文本写/读/检索不回归
- [ ] §5 cat 二进制：整文件无损、行操作报错
- [ ] §6 text→pdf 互转：旧索引清、新内容可搜、无损
