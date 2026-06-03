# 手动测试 SOP：platform 管理接口（app_id 账号 / 统一 wk_ / key 端点 / description / 砍 JWT）

> 目标：手动验证 `docs/plans/platform-admin-api-plan.md` 这批改动 + platform app_id 账号模型。
> 这些行为 `cargo check` 和 lib 单测都覆盖不到（SQL 列、真实 auth、kind 隔离、app_id 唯一约束都要真库），**必须连真实 MySQL + Milvus + embedding 跑**。
> **主线走 platform 真实路径**（非匿名）：`app_id` 建账号(无 email) → `vk_` 建 workspace + key → `wk_` 走数据面。
> 尤其验证 codex finding 1：db 数据面任何一次成功调用 = `get_active_dataset_by_name` 的 `description` 修复有效（漏列会 500）。

## 0. 前置

```bash
# 起 server（本地连内网存储；端口看 config/test.toml 的 listen，默认 3000）
cargo run -p veda-server -- config/test.toml
# 或指向已部署实例，下面 BASE 改成它即可

export BASE=http://localhost:3000      # 按需改
jq --version >/dev/null || echo "请先装 jq"

# 探针（无需 auth）
curl -s $BASE/healthz                  # 期望: ok
curl -s $BASE/v1/ready | jq            # 期望: status=ready, mysql/milvus 都 ok=true
curl -s $BASE/capabilities | jq        # 期望: data.summary_enabled
```

响应统一信封：`{ "success": bool, "data": ..., "error_code": "...", "error": "..." }`。

## 1. platform 建账号（app_id 模式，无 email/password）

```bash
# platform 为业务方建 veda 账号：只给 app_id，无 email/password
REG=$(curl -s -X POST $BASE/v1/accounts \
  -H 'content-type: application/json' \
  -d '{"name":"acme","app_id":"acme-prod"}')
echo "$REG" | jq
export VK=$(echo "$REG" | jq -r .data.api_key)
echo "VK=$VK  app_id=$(echo "$REG" | jq -r .data.app_id)"
# 期望: data.account_id + data.api_key(vk_) + data.app_id="acme-prod"
#       【没有】workspace_key —— app_id 账号不自动建 workspace（对比 anonymous）
```

✅ 验证点：返回 `vk_` + 回显 `app_id`；不带 workspace/wk_（platform 后续显式建）。

```bash
# app_id 唯一：重复建同一个 app_id → 409
curl -s -X POST $BASE/v1/accounts -H 'content-type: application/json' \
  -d '{"name":"acme-dup","app_id":"acme-prod"}' | jq '{success, error_code}'
# 期望: success=false, error_code=ALREADY_EXISTS

# 缺约束：既无 app_id 又无 email+password → 400
curl -s -X POST $BASE/v1/accounts -H 'content-type: application/json' \
  -d '{"name":"bad"}' | jq '{success, error_code}'
# 期望: success=false, error_code=INVALID_INPUT
```

> platform 后端保管这个 `vk_`（控制面凭据）。app_id 账号无 email，`vk_` 丢了 v0 没有补发路径——已知取舍（见 plan）。
> 对照：`POST /v1/accounts {name,email,password}`（email 模式，给 console/CLI）仍可用，互不影响。

## 2. 建 workspace + `description`（控制面，用 `vk_`）

```bash
# db workspace（带 description）
DB_WS=$(curl -s -X POST $BASE/v1/workspaces \
  -H "authorization: Bearer $VK" -H 'content-type: application/json' \
  -d '{"name":"vec-test","kind":"db","description":"向量库手测"}' | jq -r .data.id)

# fs workspace（带 description）—— app_id 账号不自动建，显式来一个
FS_WS=$(curl -s -X POST $BASE/v1/workspaces \
  -H "authorization: Bearer $VK" -H 'content-type: application/json' \
  -d '{"name":"files-test","kind":"fs","description":"文件库手测"}' | jq -r .data.id)
echo "DB_WS=$DB_WS  FS_WS=$FS_WS"

# 列出验证 description 回显（写入 + 读取全栈）
curl -s $BASE/v1/workspaces -H "authorization: Bearer $VK" \
  | jq '.data.items[] | {id, name, kind, description}'
# 期望: 两条都有非 null description
```

## 3. key 管理：创建 / 查看 / 删除（🆕 本次新增）

```bash
# db workspace 的 key：readwrite / read-only / 一次性
DB_WK_RW=$(curl -s -X POST $BASE/v1/workspaces/$DB_WS/keys -H "authorization: Bearer $VK" \
  -H 'content-type: application/json' -d '{"name":"sdk-rw","permission":"readwrite"}' | jq -r .data.key)
DB_WK_RO=$(curl -s -X POST $BASE/v1/workspaces/$DB_WS/keys -H "authorization: Bearer $VK" \
  -H 'content-type: application/json' -d '{"name":"sdk-ro","permission":"read"}' | jq -r .data.key)
DB_WK_DEL=$(curl -s -X POST $BASE/v1/workspaces/$DB_WS/keys -H "authorization: Bearer $VK" \
  -H 'content-type: application/json' -d '{"name":"throwaway","permission":"read"}' | jq -r .data.key)

# fs workspace 的 key（§6 数据面用）
FS_WK=$(curl -s -X POST $BASE/v1/workspaces/$FS_WS/keys -H "authorization: Bearer $VK" \
  -H 'content-type: application/json' -d '{"name":"fs-rw","permission":"readwrite"}' | jq -r .data.key)
echo "DB_RW=$DB_WK_RW  DB_RO=$DB_WK_RO  FS_WK=$FS_WK"

# 查看 db workspace 的 key 列表（🆕）
curl -s $BASE/v1/workspaces/$DB_WS/keys -H "authorization: Bearer $VK" | jq .data
```

✅ 验证点：列表返回三条，每条含 `id/name/permission/status/created_at`，**绝不含 `key_hash` 或明文 key**（明文只在创建时出现一次）。

```bash
# 取一次性 key 的 id，删除（吊销）它（🆕）
DEL_ID=$(curl -s $BASE/v1/workspaces/$DB_WS/keys -H "authorization: Bearer $VK" \
  | jq -r '.data[] | select(.name=="throwaway") | .id')
curl -s -o /dev/null -w "%{http_code}\n" -X DELETE \
  $BASE/v1/workspaces/$DB_WS/keys/$DEL_ID -H "authorization: Bearer $VK"
# 期望: 204

# 被删的 key 立即失效
curl -s -X POST $BASE/v1/vectors/search \
  -H "authorization: Bearer $DB_WK_DEL" -H 'content-type: application/json' \
  -d '{"query":"x"}' | jq '{success, error_code}'
# 期望: success=false, error_code=UNAUTHORIZED
```

✅ 验证点：删除返回 204；被删 key 立刻 401。

## 4. dataset + db 数据面（用 db `wk_`，验证 finding 1 + 请求体无 `workspace_id`）

```bash
# 建 dataset（控制面 vk_），带 description
curl -s -X POST $BASE/v1/workspaces/$DB_WS/datasets \
  -H "authorization: Bearer $VK" -H 'content-type: application/json' \
  -d '{"name":"products","description":"商品集"}' | jq '.data | {name, description, status}'
# 期望: name=products, description=商品集

# upsert：用 readwrite wk_，body 不带 workspace_id（由 wk_ 推导）
curl -s -X POST $BASE/v1/vectors/upsert \
  -H "authorization: Bearer $DB_WK_RW" -H 'content-type: application/json' \
  -d '{"dataset":"products","records":[
        {"id":"sku-1","text":"Air Jordan 1 球鞋","category":"shoes","meta":{"price":1299}},
        {"id":"sku-2","text":"夏季透气跑鞋","category":"shoes","meta":{"price":399}}]}' \
  | jq '{success, ids:.data.ids, commit_ts:.data.commit_ts, error_code}'
# 期望: success=true, ids=["sku-1","sku-2"]
# 🔴 finding 1 验证：若 success=false / 500 / error_code=INTERNAL，说明 dataset SELECT 漏列没修好

# search（hybrid 默认）
curl -s -X POST $BASE/v1/vectors/search \
  -H "authorization: Bearer $DB_WK_RW" -H 'content-type: application/json' \
  -d '{"dataset":"products","query":"运动鞋","top_k":5}' \
  | jq '.data.hits[] | {id, score, score_type}'
# 期望: 命中 sku-1/sku-2，score_type=rrf

# search + filter
curl -s -X POST $BASE/v1/vectors/search \
  -H "authorization: Bearer $DB_WK_RW" -H 'content-type: application/json' \
  -d '{"dataset":"products","query":"鞋","top_k":5,"filter":{"must":[{"field":"meta.price","op":"lt","value":500}]}}' \
  | jq '.data.hits[] | {id, meta}'
# 期望: 只命中 sku-2（price<500）

# query 按 id 直查
curl -s -X POST $BASE/v1/vectors/query \
  -H "authorization: Bearer $DB_WK_RW" -H 'content-type: application/json' \
  -d '{"dataset":"products","ids":["sku-1"]}' | jq '.data.hits[] | {id, text}'

# delete
curl -s -X POST $BASE/v1/vectors/delete \
  -H "authorization: Bearer $DB_WK_RW" -H 'content-type: application/json' \
  -d '{"dataset":"products","ids":["sku-1","sku-2"]}' | jq '.data.delete_count'
# 期望: 2
```

✅ 验证点：4 个数据面端点全部走 `wk_` + 无 `workspace_id`；finding 1 修复后不再 500。

## 5. read-only `wk_` 拒写（🔁 本次语义）

```bash
# 只读 key 能 search
curl -s -X POST $BASE/v1/vectors/search \
  -H "authorization: Bearer $DB_WK_RO" -H 'content-type: application/json' \
  -d '{"dataset":"products","query":"鞋"}' | jq '{success}'
# 期望: success=true（读放行）

# 只读 key 不能 upsert
curl -s -X POST $BASE/v1/vectors/upsert \
  -H "authorization: Bearer $DB_WK_RO" -H 'content-type: application/json' \
  -d '{"dataset":"products","records":[{"id":"x","text":"x"}]}' \
  | jq '{success, error_code}'
# 期望: success=false, error_code=PERMISSION_DENIED
```

## 6. fs 数据面（用 §2/§3 建的 fs workspace + wk_）

```bash
# 写文件
curl -s -o /dev/null -w "%{http_code}\n" -X PUT $BASE/v1/fs/hello.txt \
  -H "authorization: Bearer $FS_WK" -H 'content-type: text/plain' \
  --data-binary 'hello veda platform'                      # 期望: 200

# 读文件
curl -s $BASE/v1/fs/hello.txt -H "authorization: Bearer $FS_WK"   # 期望: hello veda platform

# 列根目录
curl -s "$BASE/v1/fs?list" -H "authorization: Bearer $FS_WK" | jq
```

## 7. kind 隔离负例（🔁 本次：fs/db 各自 `wk_` 只能走自己的通道）

```bash
# db 的 wk_ 去调 fs 端点 → 400 kind mismatch
curl -s "$BASE/v1/fs?list" -H "authorization: Bearer $DB_WK_RW" | jq '{success, error_code}'
# 期望: error_code=WORKSPACE_KIND_MISMATCH

# fs 的 wk_ 去调 vectors → 400 kind mismatch
curl -s -X POST $BASE/v1/vectors/search \
  -H "authorization: Bearer $FS_WK" -H 'content-type: application/json' \
  -d '{"query":"x"}' | jq '{success, error_code}'
# 期望: error_code=WORKSPACE_KIND_MISMATCH
```

## 8. JWT 端点已删（❌ 本次）

```bash
curl -s -o /dev/null -w "%{http_code}\n" -X POST \
  $BASE/v1/workspaces/$DB_WS/token -H "authorization: Bearer $VK"
# 期望: 404（路由已移除）
```

## 9. 清理

```bash
curl -s -o /dev/null -w "%{http_code}\n" -X DELETE $BASE/v1/workspaces/$DB_WS -H "authorization: Bearer $VK"
curl -s -o /dev/null -w "%{http_code}\n" -X DELETE $BASE/v1/workspaces/$FS_WS -H "authorization: Bearer $VK"
# 期望: 各 200
# 注：app_id 账号本身没有删除端点，测试账号会残留在测试库（无害）。
```

---

## 验证清单（对应本次改动）

| # | 验证项 | 步骤 | 期望 |
|---|---|---|---|
| 1 | **app_id 建账号**（platform 路径） | §1 | 返回 vk_ + app_id，无 workspace_key |
| 2 | app_id 唯一 / 缺约束 | §1 | 重复 app_id→409；空请求→400 |
| 3 | **finding 1**：db 数据面不 500 | §4 upsert | success=true（漏列会 INTERNAL/500）|
| 4 | key 创建/查看/删除 | §3 | 列表仅元数据无明文；删后 204 + 401 |
| 5 | `description` 全栈 | §2 §4 | workspace/dataset 创建后列表回显非 null |
| 6 | db 数据面统一 `wk_` | §4 | `wk_` + 无 `workspace_id` 可用 |
| 7 | read-only `wk_` 拒写 | §5 | search 200 / upsert PERMISSION_DENIED |
| 8 | kind 隔离 | §7 | 两个方向都 WORKSPACE_KIND_MISMATCH |
| 9 | JWT 已删 | §8 | 404 |
| 10 | fs `wk_` 仍正常 | §6 | 读写正常 |

任一项不符就是回归。第 1/2 项是这次新增的 app_id 账号模型；第 3 项最关键——它是 codex 抓的上线即崩 bug。
