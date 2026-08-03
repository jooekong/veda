# MCP 端点手动测试 SOP(测试环境)

> 对象:`POST /mcp`(Streamable HTTP,stateless,协议 2025-06-18),部署于测试节点 .161/.89。
> 入口:`https://veda.dbpaas.dingdongxiaoqu.com/mcp`(经 .161 nginx;**直连该域名**,平台网关 `paas-api-test…/proxy/veda` 只认 passport 登录态,wk_ 会 401)。
> Mac 网络:清掉 http 代理环境变量、走 Clash TUN(该域名 fake-ip 直连);curl 一律带 `--noproxy '*'`。

---

## 0. 前置:拿一把测试环境的 fs workspace key

用现有 fs workspace 的 `wk_`,或临时开一个匿名户(测试环境随开随用):

```bash
export VEDA=https://veda.dbpaas.dingdongxiaoqu.com
curl -s --noproxy '*' -X POST $VEDA/v1/accounts/anonymous | python3 -m json.tool
# 记下返回里的 workspace_key(wk_...)——默认 readwrite,§2 传语料要用它
export WK=wk_xxxxxxxx
```

推荐同时发一把 **read-only key** 验证消费者姿势(console 或 `POST /v1/workspaces/{id}/keys`,`permission=read`)。

## 1. curl 协议冒烟(不依赖任何 MCP client,3 分钟)

```bash
# 1.1 initialize:应返回 protocolVersion=2025-06-18 + serverInfo.name=veda
curl -s --noproxy '*' -H "Authorization: Bearer $WK" -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"curl","version":"0"}}}' \
  $VEDA/mcp | python3 -m json.tool

# 1.2 tools/list:应返回 6 个工具(search/grep/read_file/list_dir/overview/ask)
curl -s --noproxy '*' -H "Authorization: Bearer $WK" -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' $VEDA/mcp | python3 -c \
  "import json,sys; print([t['name'] for t in json.load(sys.stdin)['result']['tools']])"

# 1.3 负向三连
curl -s --noproxy '*' -o /dev/null -w '%{http_code}\n' -X POST -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"ping"}' $VEDA/mcp                      # 无 key → 401
curl -s --noproxy '*' -o /dev/null -w '%{http_code}\n' -H "Authorization: Bearer $WK" \
  -H 'Content-Type: application/json' -H 'MCP-Protocol-Version: 2025-03-26' \
  -d '{"jsonrpc":"2.0","id":1,"method":"ping"}' $VEDA/mcp                      # 不支持的版本 header → 400
# 用一把 db workspace 的 wk_ 调 ping → 400 WORKSPACE_KIND_MISMATCH
```

**✅ 通过标准**:1.1/1.2 结构正确;401/400 如注释。若 `/mcp` 返回 404 → 该节点还是旧 server 或 nginx 未放行此路径。

## 2. 准备测试语料

现成语料在 **`docs/sop-fixtures/mcp/`**(五种格式,每个文件一个主题 + 一个 grep 用哨兵词):

| 文件 | 主题 | 哨兵(grep 用) | ask 可问 |
|---|---|---|---|
| `deploy-guide.md` | 部署指南 | `SOP_MD_SENTINEL_71` | 「发布的灰度顺序是什么?」→ 金丝雀→全量测试→生产 |
| `oncall-handbook.txt` | 值班手册 | `SOP_TXT_SENTINEL_72` | 「P0 故障要求多久响应?」→ 五分钟 |
| `api-style.html` | API 规范 | `SOP_HTML_SENTINEL_73` | 「列表接口分页参数是什么?」→ page/size,单页上限一百 |
| `vector-intro.pdf` | 向量检索入门 | `SOP_PDF_SENTINEL_74` | 「为什么推荐混合检索?」→ 字面+语义互补 |
| `db-migration.docx` | 数据库迁移须知 | `SOP_DOCX_SENTINEL_75` | 「大表回填每批多少行?」→ 一万行以内 |
| `legacy-notes.doc` | 遗留系统备忘 | `SOP_DOC_SENTINEL_76` | 「老网关为什么不能下线?」→ 两个外部回调,明年一季度截止 |

```bash
# 上传(在 repo 根目录):
veda cp docs/sop-fixtures/mcp /mcp-test           # 目录自动递归,没有 -r;或逐个 curl PUT /v1/fs/mcp-test/...
# 约 30-60s 后(异步 embedding;pdf/word 多一道提取)用 §1 的 curl 或 REST search
# 搜任一哨兵词确认已可检索,再进 §3
```

> 注意:`api-style.html` 会连标签一起入库(HTML 暂无提取器)——read_file 读它看到标签属**已知现状**,顺带就是方案 §6 的质量实验样本,不算 bug。

## 3. Claude Code 接入(核心场景)

项目根建 `.mcp.json`(或用户级 `~/.claude.json` 的 mcpServers 段):

```json
{
  "mcpServers": {
    "veda-wiki": {
      "type": "http",
      "url": "https://veda.dbpaas.dingdongxiaoqu.com/mcp",
      "headers": { "Authorization": "Bearer wk_你的key" }
    }
  }
}
```

验证步骤:
1. 启动 `claude`,跑 `/mcp` → 应显示 `veda-wiki ✔ connected`,工具 6 个。
2. **零提示词自主调用**:直接问一个语料里才有答案的问题(如「我们 wiki 里 XX 服务的部署流程是什么?」)→ 观察 agent 自主调 `search`/`read_file`/`ask`,回答带知识库文件路径出处。
3. 显式指定:「用 veda-wiki 的 ask 工具问:XXX」→ 返回带 `[n]` 引用 + citations 的答案。

**✅ 通过标准**:连接绿、agent 无需教学自主选对工具、出处路径真实存在。

## 4. Cursor 接入

`.cursor/mcp.json` 内容同上(Cursor ≥0.46 支持 streamable http + headers)。Settings → MCP 里确认 veda-wiki 绿灯、工具可见,Chat 里同 §3 验证。

## 5. 逐工具手动测试用例

以下用例默认语料已按 §2 传到 `/mcp-test` 且已可检索。先定义一个 curl 封装(§1 的 `$VEDA`/`$WK` 已 export),之后每条用例一行命令;在 Claude Code 里测时,把「问法」直接说给 agent 即可:

```bash
mcp_call() { curl -s --noproxy '*' -H "Authorization: Bearer $WK" -H 'Content-Type: application/json' \
  -d "{\"jsonrpc\":\"2.0\",\"id\":9,\"method\":\"tools/call\",\"params\":{\"name\":\"$1\",\"arguments\":$2}}" \
  "$VEDA/mcp"; echo; }
# 结果里 result.isError=false 且 content[0].text 符合预期即通过
```

### 5.1 search

| # | arguments | 预期 |
|---|---|---|
| S1 语义改写命中 | `{"query":"上线放量要按什么顺序"}` | top hit path=`/mcp-test/deploy-guide.md`(查询一个原词没用,靠语义命中「灰度顺序」章节) |
| S2 字面词命中(BM25 路) | `{"query":"SOP_PDF_SENTINEL_74"}` | `/mcp-test/vector-intro.pdf` 进 **top-3**(验证 PDF 文本层已入索引 + 字面一路工作)。不要求 top-1:六个哨兵句式高度相似,dense 一路对它们无区分度,RRF 融合会稀释字面强命中——2026-07-22 实测 fulltext 单路 pdf 以 40.7 分断层第一、hybrid 里第 3,属 RRF 正常行为;**要精确定位用 grep** |
| S3 abstract 分层 | `{"query":"值班 故障 响应","detail_level":"abstract","limit":3}` | 每个 hit 带 `l0_abstract` 一句话摘要(**摘要异步生成,上传后 1-3 分钟内可能为空,稍后重试**) |
| S4 子树过滤 | `{"query":"灰度顺序","path_prefix":"/mcp-test"}` | 正常命中;换 `"path_prefix":"/nowhere"` → 空数组 `[]` |

### 5.2 grep

| # | arguments | 预期 |
|---|---|---|
| G1 精确定位 | `{"pattern":"SOP_MD_SENTINEL_71"}` | 恰 1 hit:path=`/mcp-test/deploy-guide.md`,`line_no:3` |
| G2 大小写 | `{"pattern":"sop_txt_sentinel_72","ignore_case":true}` | 命中 `/mcp-test/oncall-handbook.txt`,`line_no:2`;不带 ignore_case → `[]` |
| G3 长行截断(可选) | 先 `python3 -c "print('LONGLINE_MARK '+'x'*3000)" \| veda cp - /mcp-test/long.txt`,再 `{"pattern":"LONGLINE_MARK"}` | 命中行被截到 ~500B 且以 `…` 结尾 |

### 5.3 read_file

| # | arguments | 预期 |
|---|---|---|
| R1 Word 提取 | `{"path":"/mcp-test/db-migration.docx"}` | **中文提取文本**非乱码,含「迁移三原则」「每批一万行」 |
| R2 老 .doc 提取 | `{"path":"/mcp-test/legacy-notes.doc"}` | 提取文本含「老网关」「SOP_DOC_SENTINEL_76」 |
| R3 PDF 提取 | `{"path":"/mcp-test/vector-intro.pdf"}` | 提取文本含「混合检索」 |
| R4 行范围 | `{"path":"/mcp-test/deploy-guide.md","start_line":1,"end_line":3}` | 只有标题+哨兵行,**不含**「灰度顺序」正文 |
| R5 HTML 现状 | `{"path":"/mcp-test/api-style.html"}` | 返回**带标签**的原文(已知现状,见 §2 注记,不算 bug) |
| R6 缺文件 | `{"path":"/mcp-test/nope.md"}` | `isError:true` + not found 文案 |
| R7 参数校验 | `{"path":"/mcp-test/deploy-guide.md","end_line":5}`(无 start_line) | JSON-RPC error `-32602` |

### 5.4 list_dir

| # | arguments | 预期 |
|---|---|---|
| L1 平铺 | `{"path":"/mcp-test"}` | `entries` 恰 6 个文件(+可选 long.txt),`truncated:false` |
| L2 递归 | `{"recursive":true}` | 完整子树含 6 个 `/mcp-test/...` 路径,`truncated:false` |

### 5.5 overview

| # | arguments | 预期 |
|---|---|---|
| O1 pending 时序 | 上传后立刻 `{"path":"/mcp-test/deploy-guide.md"}` | `isError:true` +「not ready yet…retry」(摘要 30s 防抖 + LLM 异步,**属正常**) |
| O2 就绪 | 1-3 分钟后重试同一调用 | `l1_overview` 结构化概览(含章节要点) |
| O3 目录聚合 | `{"path":"/mcp-test"}`(再等几分钟) | 目录级 L1(自底向上聚合,比文件级更慢) |

### 5.6 ask

| # | arguments | 预期 |
|---|---|---|
| A1 单文档事实 | `{"question":"P0 故障要求多久响应?"}` | 答案含「五分钟」+ 内联 `[n]`;citations 含 `/mcp-test/oncall-handbook.txt` |
| A2 跨文档综合 | `{"question":"新人要接手部署和值班,有哪些不能踩的红线?"}` | 综合 deploy-guide + oncall(可能含 db-migration),citations ≥2 条不同文件 |
| A3 Word 内容进答案 | `{"question":"大表数据回填每批控制在多少行?","path_prefix":"/mcp-test"}` | 答案含「一万行」,citations 含 docx——验证 Word 提取文本进了 RAG 链路 |
| A4 拒答不编造 | `{"question":"明天上海天气怎么样?"}` | 固定拒答话术,citations 空(**不得**编造答案) |
| A5 并发闸 | `for i in 1 2 3; do mcp_call ask '{"question":"P0 响应时限?"}' & done; wait` | 至少 1 个返回 `isError:true`「too many concurrent」(每 workspace 并发上限 2);其余正常 |

### 5.7 read-only key 复跑

换 §0 的 **read-only wk_** 把 S1/G1/R1/L1/O2/A1 各跑一遍 → 行为与读写 key 完全一致(全工具只读,这是推荐发给消费者的 key 姿势)。

## 6. 观察面

```bash
# 节点侧(ssh 进 .161/.89):
journalctl -u veda-server --since "10 min ago" | grep -iE "error|panic" | tail   # 应无新 error
curl -s localhost:3000/v1/metrics -H "Authorization: Bearer $METRICS_TOKEN" | grep veda_mcp
# → veda_mcp_request_seconds{method="tool:search",outcome="ok"} 等按调用增长
```

## 7. 常见问题

| 症状 | 原因 |
|---|---|
| `/mcp` 404 | 旧 server 未换,或 nginx 没把 /mcp 转给后端(只放行了 /v1 前缀) |
| curl 卡住/连不上 | mac http 代理没清;必须 `--noproxy '*'` + Clash TUN 直连该域名 |
| Claude Code 连接失败但 curl 通 | headers 里 key 打错;或客户端要求 `type:"http"` 字段缺失 |
| ask 一直 isError「disabled」 | 该节点 [llm] 未配置(测试节点应已配 airouter,出现即环境问题) |
| ask 偶发 429/「too many concurrent」 | answer_concurrency=2 的并发闸,正常保护行为 |
