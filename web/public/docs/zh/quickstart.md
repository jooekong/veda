# 快速开始

Veda 有两种 workspace，先想清楚你要哪种，再跟对应那一节走：

- **文件库**（`kind=fs`）—— 存文件、自动嵌入、语义搜索 + SQL，可以挂成本地目录。个人知识库、Agent 记忆、代码搜索走这条，用 CLI 最顺手。
- **向量库**（`kind=db`）—— Pinecone 式托管向量检索，写文本、服务端嵌入、按语义检索。业务应用接 REST / SDK 走这条。

---

## A. 文件库：CLI 两分钟上手

### 1. 拿账号

在 [首页](/) 点 **Get started anonymously**，页面会给你三样东西：

- `vk_xxx` —— **账号 key**，管理 workspace 用。
- `wk_xxx` —— **workspace key**，文件 / 搜索操作用。
- 一个 workspace id。

`wk_` **只显示一次**，离开页面前把两个 key 都复制到安全的地方。后面可以在 **Console** 点 **Claim account** 加邮箱密码，把匿名账号升级成正式账号。

### 2. 装 CLI

```bash
curl -fsSL https://veda.dbpaas.dingdongxiaoqu.com/install.sh | sh
```

二进制装到 `/usr/local/bin/`（root）或 `~/.local/bin/`（非 root）。重开终端让它进 `PATH`，然后 `veda --help` 验证。

### 3. 连上你的账号

```bash
veda init --server https://veda.dbpaas.dingdongxiaoqu.com --import-key vk_xxx
```

配置写到 `~/.config/veda/config.toml`（已有文件会先备份成 `config.toml.bak.<时间戳>`）。

### 4. 上传、读、列

```bash
echo "hello veda" > /tmp/hi.txt
veda cp /tmp/hi.txt /hi.txt        # 上传：本地 → 远端
veda ls
veda cat /hi.txt                   # 读远端文件（下载就是 veda cat /远端 > 本地）
```

服务端路径都是 workspace 根下的绝对路径（`/`）。

### 5. 搜索

嵌入是**异步**的，刚上传等几秒；没命中过 5 秒再试。

```bash
veda search "greeting"             # 默认 hybrid（向量 + BM25 + RRF）
veda search "hello" --mode fulltext
veda search "concept" --mode semantic
veda grep "hello"                  # 字面匹配，同步无延迟，输出 file:line
```

### 6. 当数据库查

文件可以当虚拟表跑 SQL：

```bash
veda sql "SELECT path, size_bytes FROM files ORDER BY created_at DESC LIMIT 5"
```

**下一步**：[CLI 速查](#/docs/cli)（完整命令）· [FUSE 挂载](#/docs/fuse)（挂成本地目录）· [详细文档](#/docs/reference)

---

## B. 向量库：业务接入

向量库面向业务 app，走 HTTP / SDK。`<BASE>` 用部署地址，示例：`https://veda.dbpaas.dingdongxiaoqu.com`。

业务 app 通常**只拿一把 `wk_`**（平台 / 控制台为某个 db workspace 签发）。拿到后直接打数据面：

```bash
BASE=https://veda.dbpaas.dingdongxiaoqu.com
WK=wk_...        # 平台发给你的 workspace key（请求体不带 workspace_id）

# 写入：text 由服务端自动嵌入
curl -sX POST $BASE/v1/vectors/upsert \
  -H "authorization: Bearer $WK" -H 'content-type: application/json' \
  -d '{"records":[
        {"id":"sku-1","text":"Air Jordan 1","meta":{"price":1299}},
        {"id":"sku-2","text":"running shoes","meta":{"price":499}}
      ]}'

# 检索：默认 hybrid（向量 + BM25），可加 meta 过滤
curl -sX POST $BASE/v1/vectors/search \
  -H "authorization: Bearer $WK" -H 'content-type: application/json' \
  -d '{"query":"sneakers under 1500","top_k":5,
       "filter":{"must":[{"field":"meta.price","op":"lt","value":1500}]}}'
```

读分数前先看 `score_type`（`rrf` / `cosine` / `bm25` 跨 type 不可比）。

> ⚠️ 别用匿名 onboarding（`POST /v1/accounts/anonymous`）接向量库——它建的是 `kind=fs`，向量端点用不了。

<details>
<summary>自己开通一个向量库（持账号 vk_）</summary>

如果你手上是账号 `vk_`、要自己建库并签 key：

```bash
ACCOUNT_KEY=vk_...

# 建一个 db workspace（自动 bootstrap default dataset + Milvus collection）
curl -sX POST $BASE/v1/workspaces \
  -H "authorization: Bearer $ACCOUNT_KEY" -H 'content-type: application/json' \
  -d '{"name":"prod-index","kind":"db"}'        # → data.id 即 <ws_id>

# 为它签一把数据面 wk_（明文仅此一次）
curl -sX POST $BASE/v1/workspaces/<ws_id>/keys \
  -H "authorization: Bearer $ACCOUNT_KEY" -H 'content-type: application/json' \
  -d '{"name":"search-svc","permission":"readwrite"}'
```

</details>

**下一步**：[向量库 API](#/docs/vectors)（完整接口 / Filter DSL / 限制）· [详细文档](#/docs/reference)

---

## 卡住了？

看 [常见问题](#/docs/troubleshooting)，或去 [git.ddxq.mobi/middleware/dbpaas/veda](http://git.ddxq.mobi/middleware/dbpaas/veda) 提 issue。
