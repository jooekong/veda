# Vectors API 本地 Dogfood SOP

> 覆盖 vss → Veda merge 全部新增能力（HEAD `92eb5c0` 截止）：db-kind workspace、dataset CRUD、vectors upsert/search/query/delete、Filter DSL、admin tokens、长 text 边界、auth scope、错误码、L1 embedding cache 观察。
>
> curl 为主，Python demo（`examples/python_pinecone_demo.py`）在 §3 作为烟测复用。所有 curl 例子用 shell 变量串起来，按顺序粘贴即可。

---

## 1. 前置

### 1.1 服务依赖

确认下列三个外部服务可达（VPN 已联）：

- **MySQL**：`10.78.81.148:3306/vecfs`（账号在 `config/server.toml [mysql]`）
- **Milvus 2.6.14**：`milvus-dist.test.srv.mc.dd:19530`（token 在 config）
- **Embedding**：`airouter.ddmc-inc.com/api/v1/embeddings`（model `text-embedding-v4`，dim 1024）

```bash
mysql -h 10.78.81.148 -urw_dbpaas -p'GpoqEDWg80vO' -e "SELECT 1" vecfs
curl -s -m 3 http://milvus-dist.test.srv.mc.dd:19530/healthz || echo "Milvus unreachable"
```

### 1.2 编译 + 启动

```bash
cd /Users/konglingqiao/code/personal/veda
cargo build -p veda-server   # ~1 min cold
cargo run -p veda-server config/server.toml
```

启动成功日志应包含：
```
running schema bootstrap (CREATE TABLE IF NOT EXISTS)
retention sweep enabled (fs_events + outbox)
reconciler enabled
server listening
```

新开 shell 跑下面所有 curl。Server 留前台看日志（dogfood 时尤其要看 embedding cache 和 Milvus error）。

### 1.3 Shell 变量约定

```bash
export VEDA_URL="http://localhost:3000"
# 后面会依次填充：
# VEDA_ACCT_KEY     vk_... 账号根 key（能签 admin token、建 workspace）
# VEDA_WS_ID        db-kind workspace UUID
# VEDA_APP_KEY      admin token 签出的 app vk_，scope 到 VEDA_WS_ID
```

---

## 2. 凭证 Bootstrap

### 2.1 创建账号

```bash
RESP=$(curl -sS -X POST "$VEDA_URL/v1/accounts" \
  -H 'Content-Type: application/json' \
  -d '{"name":"dogfood","email":"dogfood@local.test","password":"test1234"}')
echo "$RESP" | python3 -m json.tool --no-ensure-ascii

export VEDA_ACCT_KEY=$(echo "$RESP" | python3 -c "import sys,json;print(json.load(sys.stdin)['data']['api_key'])")
echo "ACCT_KEY=$VEDA_ACCT_KEY"
```

预期：返回 `account_id` + `api_key`（vk_ 开头）。如已存在 email，返 409 `already exists`。重复跑请换 email 或 `mysql ... -e "DELETE FROM veda_accounts WHERE email='dogfood@local.test'"`。

### 2.2 创建 db-kind workspace

```bash
RESP=$(curl -sS -X POST "$VEDA_URL/v1/workspaces" \
  -H "Authorization: Bearer $VEDA_ACCT_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"name":"dogfood-vec","kind":"db","app_id":"dogfood-app"}')
echo "$RESP" | python3 -m json.tool --no-ensure-ascii

export VEDA_WS_ID=$(echo "$RESP" | python3 -c "import sys,json;print(json.load(sys.stdin)['data']['id'])")
echo "WS_ID=$VEDA_WS_ID"
```

预期：返回 `kind:"db"` 的 workspace。workspace 行 + default dataset 行现在是**单事务原子提交**（`create_db_workspace`），随后服务端建 Milvus collection（`provision_db_collection`）；任一步失败会回滚这两行并 drop collection，不会留下"能 list 但 upsert 404"的孤儿 workspace。

### 2.3 用 admin token 接口签 app token（scope 收窄）

```bash
# epoch ms = now + 24h. macOS 的 BSD date 不支持 %N，用 python3 算（跨平台）
EXPIRES_AT=$(python3 -c "import time; print(int(time.time()*1000) + 86400000)")

RESP=$(curl -sS -X POST "$VEDA_URL/admin/v1/tokens" \
  -H "Authorization: Bearer $VEDA_ACCT_KEY" \
  -H 'Content-Type: application/json' \
  -d "{
    \"app_id\": \"dogfood-app\",
    \"name\": \"dogfood-rw\",
    \"allowed_workspaces\": [\"$VEDA_WS_ID\"],
    \"expires_at\": $EXPIRES_AT
  }")
echo "$RESP" | python3 -m json.tool --no-ensure-ascii

export VEDA_APP_KEY=$(echo "$RESP" | python3 -c "import sys,json;print(json.load(sys.stdin)['data']['token'])")
export VEDA_TOKEN_ID=$(echo "$RESP" | python3 -c "import sys,json;print(json.load(sys.stdin)['data']['id'])")
echo "APP_KEY=$VEDA_APP_KEY  TOKEN_ID=$VEDA_TOKEN_ID"
```

`expires_at` = 当前时间 + 24h（epoch ms）。预期 201。后面数据面用 `$VEDA_APP_KEY`，根 key `$VEDA_ACCT_KEY` 留给控制面。

---

## 3. 烟测：跑 Python demo

```bash
VEDA_URL="$VEDA_URL" \
  VEDA_API_KEY="$VEDA_APP_KEY" \
  VEDA_WS_ID="$VEDA_WS_ID" \
  python3 examples/python_pinecone_demo.py
```

预期输出（前 5 行）：
```
upsert: {'inserted': [...], 'commit_ts': ...}
  hit: sku-1 score=... meta={'price': 1299}
query hits: ['sku-1', 'sku-2']
delete: {'delete_count': 2}
```

烟测过了就进入分项验证。

---

## 4. 数据面深入测试（curl）

> **read-your-writes（C1 验证点）**：本批 commit 起 db 读路径（search / query）
> 强制 `consistencyLevel: Strong`。所以下面所有 upsert→search、delete→search
> 序列都应**立即一致**——刚 upsert 的数据必须当场搜到，刚 delete 的必须当场消失。
> 任何"刚写却查不到 / 已删却还在"都是 C1 的回归，**不要**用 sleep/重试掩盖。

### 4.1 Upsert：默认 dataset + 命名 dataset

```bash
# 4.1.a 默认 dataset（不指定）
curl -sS -X POST "$VEDA_URL/v1/vectors/upsert" \
  -H "Authorization: Bearer $VEDA_APP_KEY" \
  -H 'Content-Type: application/json' \
  -d "{
    \"workspace_id\":\"$VEDA_WS_ID\",
    \"records\":[
      {\"id\":\"a-1\",\"text\":\"球鞋 Air Jordan 1\",\"meta\":{\"price\":1299,\"brand\":\"nike\"},\"tags\":[\"sale\"]},
      {\"id\":\"a-2\",\"text\":\"Yeezy 350 灰色\",\"meta\":{\"price\":1599,\"brand\":\"adidas\"}}
    ]
  }" | python3 -m json.tool --no-ensure-ascii

# 4.1.b 命名 dataset（先建 dataset，§6 也会再用）
curl -sS -X POST "$VEDA_URL/v1/workspaces/$VEDA_WS_ID/datasets" \
  -H "Authorization: Bearer $VEDA_APP_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"name":"products"}' | python3 -m json.tool --no-ensure-ascii

# upsert 到命名 dataset
curl -sS -X POST "$VEDA_URL/v1/vectors/upsert" \
  -H "Authorization: Bearer $VEDA_APP_KEY" \
  -H 'Content-Type: application/json' \
  -d "{
    \"workspace_id\":\"$VEDA_WS_ID\",
    \"dataset\":\"products\",
    \"records\":[
      {\"id\":\"p-1\",\"text\":\"耳机 AirPods Pro\",\"meta\":{\"price\":1899,\"brand\":\"apple\"}},
      {\"id\":\"p-2\",\"text\":\"手表 Apple Watch\",\"meta\":{\"price\":2999,\"brand\":\"apple\"}},
      {\"id\":\"p-3\",\"text\":\"键盘 HHKB\",\"meta\":{\"price\":2200,\"brand\":\"pfu\"}}
    ]
  }" | python3 -m json.tool --no-ensure-ascii
```

预期：每次返 `inserted` 数组 + `commit_ts`（毫秒）。

### 4.2 Search：纯语义 + Filter DSL 全部 op

```bash
# 4.2.a 纯语义（默认 dataset）
curl -sS -X POST "$VEDA_URL/v1/vectors/search" \
  -H "Authorization: Bearer $VEDA_APP_KEY" \
  -H 'Content-Type: application/json' \
  -d "{\"workspace_id\":\"$VEDA_WS_ID\",\"query\":\"运动鞋\",\"top_k\":5}" | python3 -m json.tool --no-ensure-ascii

# 4.2.b filter: eq（按 brand）
curl -sS -X POST "$VEDA_URL/v1/vectors/search" \
  -H "Authorization: Bearer $VEDA_APP_KEY" \
  -H 'Content-Type: application/json' \
  -d "{
    \"workspace_id\":\"$VEDA_WS_ID\",
    \"dataset\":\"products\",
    \"query\":\"苹果电子产品\",
    \"filter\":{\"must\":[{\"field\":\"meta.brand\",\"op\":\"eq\",\"value\":\"apple\"}]}
  }" | python3 -m json.tool --no-ensure-ascii

# 4.2.c filter: in（OR 展开）
curl -sS -X POST "$VEDA_URL/v1/vectors/search" \
  -H "Authorization: Bearer $VEDA_APP_KEY" \
  -H 'Content-Type: application/json' \
  -d "{
    \"workspace_id\":\"$VEDA_WS_ID\",
    \"dataset\":\"products\",
    \"query\":\"配件\",
    \"filter\":{\"must\":[{\"field\":\"meta.brand\",\"op\":\"in\",\"value\":[\"apple\",\"pfu\"]}]}
  }" | python3 -m json.tool --no-ensure-ascii

# 4.2.d filter: range 组合（gte + lt）
curl -sS -X POST "$VEDA_URL/v1/vectors/search" \
  -H "Authorization: Bearer $VEDA_APP_KEY" \
  -H 'Content-Type: application/json' \
  -d "{
    \"workspace_id\":\"$VEDA_WS_ID\",
    \"dataset\":\"products\",
    \"query\":\"中等价位\",
    \"filter\":{\"must\":[
      {\"field\":\"meta.price\",\"op\":\"gte\",\"value\":1800},
      {\"field\":\"meta.price\",\"op\":\"lt\",\"value\":2500}
    ]}
  }" | python3 -m json.tool --no-ensure-ascii
# 预期：返回 p-1 (1899) + p-3 (2200)，过滤掉 p-2 (2999)
```

### 4.3 Query by id（不走语义）

```bash
# 包含一个不存在的 id（应被静默忽略）
curl -sS -X POST "$VEDA_URL/v1/vectors/query" \
  -H "Authorization: Bearer $VEDA_APP_KEY" \
  -H 'Content-Type: application/json' \
  -d "{\"workspace_id\":\"$VEDA_WS_ID\",\"dataset\":\"products\",\"ids\":[\"p-1\",\"p-2\",\"p-missing\"]}" \
  | python3 -m json.tool --no-ensure-ascii
# 预期：hits 长度 2（p-missing 不在结果里，但不报错）
```

### 4.4 Delete + 二次 search 验证消失

```bash
curl -sS -X POST "$VEDA_URL/v1/vectors/delete" \
  -H "Authorization: Bearer $VEDA_APP_KEY" \
  -H 'Content-Type: application/json' \
  -d "{\"workspace_id\":\"$VEDA_WS_ID\",\"dataset\":\"products\",\"ids\":[\"p-3\"]}" \
  | python3 -m json.tool --no-ensure-ascii

# 二次 search 同样过滤 brand=pfu 时应返 0 hits（p-3 已删）
curl -sS -X POST "$VEDA_URL/v1/vectors/search" \
  -H "Authorization: Bearer $VEDA_APP_KEY" \
  -H 'Content-Type: application/json' \
  -d "{
    \"workspace_id\":\"$VEDA_WS_ID\",\"dataset\":\"products\",\"query\":\"键盘\",
    \"filter\":{\"must\":[{\"field\":\"meta.brand\",\"op\":\"eq\",\"value\":\"pfu\"}]}
  }" | python3 -m json.tool --no-ensure-ascii
```

✅ search/query 现在走 `consistencyLevel: Strong`，delete 后第二个 search 应**立即**返回 0 hits（p-3 已删）。若仍看到 p-3，是 read-your-writes 回归（C1），按 §4 顶部提示处理，不要用"等 1-2s"绕过。

---

## 5. I1 长 text 边界验证（本次 commit 重点）

### 5.1 60 KB 中文 → 接受

```bash
# 生成 ~60KB 中文 text（中文 UTF-8 = 3 bytes/char）。export 让子进程能读到。
export LONG_TEXT=$(python3 -c "print('中文示例。' * 3300, end='')")  # 19800 chars × 3 bytes ≈ 59.4KB
echo "长度: $(echo -n "$LONG_TEXT" | wc -c) bytes"

curl -sS -X POST "$VEDA_URL/v1/vectors/upsert" \
  -H "Authorization: Bearer $VEDA_APP_KEY" \
  -H 'Content-Type: application/json' \
  -d "$(python3 -c "
import json, os
print(json.dumps({
    'workspace_id': os.environ['VEDA_WS_ID'],
    'dataset': 'products',
    'records': [{'id':'long-1','text':os.environ['LONG_TEXT'],'meta':{'is_long': True}}]
}))")" | python3 -m json.tool --no-ensure-ascii

# BM25 搜回
curl -sS -X POST "$VEDA_URL/v1/vectors/search" \
  -H "Authorization: Bearer $VEDA_APP_KEY" \
  -H 'Content-Type: application/json' \
  -d "{\"workspace_id\":\"$VEDA_WS_ID\",\"dataset\":\"products\",\"query\":\"中文示例\",\"top_k\":3}" \
  | python3 -m json.tool --no-ensure-ascii
# 预期：long-1 在 hits 里
```

### 5.2 70 KB → 拒绝（payload_too_large）

```bash
export TOO_LONG=$(python3 -c "print('A' * 70000, end='')")  # 70KB 纯 ASCII，肯定超 65535
curl -sS -X POST "$VEDA_URL/v1/vectors/upsert" \
  -H "Authorization: Bearer $VEDA_APP_KEY" \
  -H 'Content-Type: application/json' \
  -d "$(python3 -c "
import json, os
print(json.dumps({
    'workspace_id': os.environ['VEDA_WS_ID'],
    'records': [{'id':'too-long','text':os.environ['TOO_LONG']}]
}))")" | python3 -m json.tool --no-ensure-ascii
# 预期：error 包含 "invalid input: text: exceeds 65535 bytes"，HTTP 400
```

---

## 6. Dataset 控制面

### 6.1 List / Create / Delete

```bash
# List：应至少有 default + products
curl -sS "$VEDA_URL/v1/workspaces/$VEDA_WS_ID/datasets" \
  -H "Authorization: Bearer $VEDA_APP_KEY" | python3 -m json.tool --no-ensure-ascii

# Create 临时 dataset
curl -sS -X POST "$VEDA_URL/v1/workspaces/$VEDA_WS_ID/datasets" \
  -H "Authorization: Bearer $VEDA_APP_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"name":"throwaway"}' | python3 -m json.tool --no-ensure-ascii

# Soft-delete
curl -sS -X DELETE "$VEDA_URL/v1/workspaces/$VEDA_WS_ID/datasets/throwaway" \
  -H "Authorization: Bearer $VEDA_APP_KEY" -w "HTTP %{http_code}\n"
# 预期：204 No Content

# Re-list 应不见 throwaway
curl -sS "$VEDA_URL/v1/workspaces/$VEDA_WS_ID/datasets" \
  -H "Authorization: Bearer $VEDA_APP_KEY" | python3 -m json.tool --no-ensure-ascii
```

### 6.2 默认 dataset 不可删

```bash
curl -sS -X DELETE "$VEDA_URL/v1/workspaces/$VEDA_WS_ID/datasets/default" \
  -H "Authorization: Bearer $VEDA_APP_KEY" -w "\nHTTP %{http_code}\n"
# 预期：400，body 含 "cannot delete the default dataset"
```

### 6.3 Dataset 名大小写不敏感（MySQL ai_ci）

```bash
# 建 "Foo"
curl -sS -X POST "$VEDA_URL/v1/workspaces/$VEDA_WS_ID/datasets" \
  -H "Authorization: Bearer $VEDA_APP_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"name":"Foo"}' | python3 -m json.tool --no-ensure-ascii

# 再建 "foo" → 应 409 already exists
curl -sS -X POST "$VEDA_URL/v1/workspaces/$VEDA_WS_ID/datasets" \
  -H "Authorization: Bearer $VEDA_APP_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"name":"foo"}' -w "\nHTTP %{http_code}\n"

# upsert 用 "foo"（小写）应 hit "Foo" 那条 dataset row（canonicalize）
curl -sS -X POST "$VEDA_URL/v1/vectors/upsert" \
  -H "Authorization: Bearer $VEDA_APP_KEY" \
  -H 'Content-Type: application/json' \
  -d "{\"workspace_id\":\"$VEDA_WS_ID\",\"dataset\":\"foo\",\"records\":[{\"id\":\"f1\",\"text\":\"case test\"}]}" \
  | python3 -m json.tool --no-ensure-ascii

# 用 "Foo" 查回（一致性证明）
curl -sS -X POST "$VEDA_URL/v1/vectors/query" \
  -H "Authorization: Bearer $VEDA_APP_KEY" \
  -H 'Content-Type: application/json' \
  -d "{\"workspace_id\":\"$VEDA_WS_ID\",\"dataset\":\"Foo\",\"ids\":[\"f1\"]}" | python3 -m json.tool --no-ensure-ascii
```

---

## 7. Auth scope

### 7.1 db workspace 用 fs API → 400

```bash
# fs 类 API（这里挑 GET /v1/fs，需要 wk_ workspace key 而不是 vk_，本步只是验 kind 检查）
curl -sS "$VEDA_URL/v1/fs/" -H "Authorization: Bearer $VEDA_APP_KEY" -w "\nHTTP %{http_code}\n"
# 预期：4xx；body 不一定是 workspace_kind_mismatch，因为 fs API 用 AuthWorkspace 而非 AuthAccount，
# 这一步主要确认 vk_ token 不能直接打 fs 路径
```

### 7.2 越权：用一个不在 allowed_workspaces 的 ws_id 调 vectors API

```bash
# 用 ACCT_KEY 再建一个 workspace，故意不在 APP_KEY 的 allowed_workspaces 里
OTHER=$(curl -sS -X POST "$VEDA_URL/v1/workspaces" \
  -H "Authorization: Bearer $VEDA_ACCT_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"name":"other-ws","kind":"db"}' | python3 -c "import sys,json;print(json.load(sys.stdin)['data']['id'])")

# 用 APP_KEY (scope 限于 VEDA_WS_ID) 去访问 OTHER → 应 403
curl -sS -X POST "$VEDA_URL/v1/vectors/search" \
  -H "Authorization: Bearer $VEDA_APP_KEY" \
  -H 'Content-Type: application/json' \
  -d "{\"workspace_id\":\"$OTHER\",\"query\":\"test\"}" -w "\nHTTP %{http_code}\n"
# 预期：403 permission denied

# 清理
curl -sS -X DELETE "$VEDA_URL/v1/workspaces/$OTHER" \
  -H "Authorization: Bearer $VEDA_ACCT_KEY" -w "\nHTTP %{http_code}\n"
```

### 7.3 错 workspace_id → 404

```bash
curl -sS -X POST "$VEDA_URL/v1/vectors/search" \
  -H "Authorization: Bearer $VEDA_APP_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"workspace_id":"00000000-0000-0000-0000-000000000000","query":"x"}' \
  -w "\nHTTP %{http_code}\n"
# 预期：4xx（403 或 404，取决于 scope 检查顺序）
```

---

## 8. Admin tokens 撤销

### 8.1 Disable token

```bash
curl -sS -X POST "$VEDA_URL/admin/v1/tokens/$VEDA_TOKEN_ID/disable" \
  -H "Authorization: Bearer $VEDA_ACCT_KEY" -w "\nHTTP %{http_code}\n"
# 预期：204
```

### 8.2 用已 disable 的 token 调数据面 → 401

```bash
curl -sS -X POST "$VEDA_URL/v1/vectors/search" \
  -H "Authorization: Bearer $VEDA_APP_KEY" \
  -H 'Content-Type: application/json' \
  -d "{\"workspace_id\":\"$VEDA_WS_ID\",\"query\":\"x\"}" -w "\nHTTP %{http_code}\n"
# 预期：401 unauthorized
```

### 8.3 跨账号 disable → 404（不泄露存在性）

略（需要建第二个账号，dogfood 阶段非必测）。

---

## 9. 观察

### 9.1 Embedding cache hit/miss（server 日志）

回到 server 前台。重复用**相同 text** upsert/search 几次：

```bash
# 第一次 search "球鞋"
curl -sS -X POST "$VEDA_URL/v1/vectors/search" \
  -H "Authorization: Bearer $VEDA_ACCT_KEY" \
  -H 'Content-Type: application/json' \
  -d "{\"workspace_id\":\"$VEDA_WS_ID\",\"query\":\"球鞋\"}" > /dev/null

# 立即再 search "球鞋"（应命中 L1 cache，server 不再打 embedding API）
curl -sS -X POST "$VEDA_URL/v1/vectors/search" \
  -H "Authorization: Bearer $VEDA_ACCT_KEY" \
  -H 'Content-Type: application/json' \
  -d "{\"workspace_id\":\"$VEDA_WS_ID\",\"query\":\"球鞋\"}" > /dev/null
```

server 日志里观察：第一次会有 `POST .../embeddings` 类似条目（tracing 的 HTTP 调用日志），第二次没有 → cache hit。

> **已知**：backlog I6 列出 embedding cache 当前**没有 metrics**，只能靠 log 间接观察。dogfood 验证完后 I6 会补 `embed_cache_hits / embed_cache_misses` counter。

### 9.2 /v1/metrics（默认未开 token 时返 404，需要 config 加 metrics_token）

如果 `config/server.toml` 加了 `metrics_token = "..."`：
```bash
curl -sS "$VEDA_URL/v1/metrics" -H "Authorization: Bearer YOUR_METRICS_TOKEN" | grep -E "veda_(fs|mysql_pool)" | head
```

未加 token 则跳过这步。

---

## 10. 清理

```bash
# Archive workspace（soft-delete；datasets 也是 soft，可后台清表）
curl -sS -X DELETE "$VEDA_URL/v1/workspaces/$VEDA_WS_ID" \
  -H "Authorization: Bearer $VEDA_ACCT_KEY" -w "\nHTTP %{http_code}\n"

# 如果要彻底清账号（手动 mysql；不暴露 API）
# mysql ... -e "DELETE FROM veda_accounts WHERE email='dogfood@local.test'"
```

注意：backlog C4 明文 "workspace 软删不级联 datasets" 是个**已知 inconsistency**，dogfood 期间 archive workspace 后 `veda_datasets` 仍会留 `active` 行。本 SOP 不验证级联（C4 修了再补）。

---

## 出错排查速查

| 现象 | 多半原因 | 处理 |
|---|---|---|
| 启动报 Milvus connect refused | VPN 没连 / 公司内网 DNS | 重连 VPN |
| Upsert 报 `dim mismatch` | `config.embedding.dimension` 改过而 collection 是旧 dim | archive 该 ws + 重建 (per backlog C2 决定) |
| `409 already exists: dataset Foo` 而你用的是 "foo" | MySQL ai_ci 大小写不敏感（§6.3） | 用规范名，或换名 |
| 长 text 拒绝 `exceeds 65535 bytes` | 单条 record > 64 KiB UTF-8 | client chunk |
| `permission denied` 503 → 应为 403 | scope 不包含目标 workspace | 用 `$VEDA_ACCT_KEY` 而非受限的 `$VEDA_APP_KEY` |
| Embedding 调用 5s+ 才返 | airouter 上游慢，不是 Veda 问题 | 等或换 model |

---

## 完整执行约 20-30 分钟（不含 §3 Python demo 准备）

跑完后建议把下列内容写到 ops 笔记里：
- 哪一步 server 日志最吵（debug 级会被刷屏）
- L1 cache 在你的 text 模式下命中率主观感受
- 哪个 endpoint 响应最慢（dogfood 时关注是否需要加超时/熔断）
