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

```bash
# 传几个真实 wiki 页(md/docx/pdf 混合),等索引完成
veda cp -r ./wiki-sample /mcp-test        # 或 curl PUT /v1/fs/...
# 约 30-60s 后(异步 embedding)用 REST search 确认可搜,再进 §3
```

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

## 5. 逐工具验证清单

| # | 工具 | 动作 | 预期 |
|---|---|---|---|
| 1 | search | 问语料里的语义问题(换个说法,别用原词) | 命中正确文件;`detail_level:"abstract"` 时每 hit 带 l0_abstract |
| 2 | grep | 搜语料里的精确标识符 | 返回 path + 1-indexed line_no;超长行被截到 ~500B 加 `…` |
| 3 | read_file | 读一个 docx/pdf 的 path | 返回**提取文本**非乱码;>64KB 文件被截断且提示用 start_line 分页 |
| 4 | read_file | `start_line:1, end_line:5` | 只返回前 5 行 |
| 5 | list_dir | `path:"/", recursive:true` | 完整子树 `{entries,truncated:false}` |
| 6 | overview | 传一个已索引文件 path | 返回 l1_overview;刚传的文件可能返回「not ready yet」(isError,合理) |
| 7 | ask | 开放问题 | 10-90s 返回带 `[n]` 引用答案;并发 >2 时第 3 个返回「too many concurrent」 |
| 8 | 全部 | 换 **read-only wk_** 重跑 1-7 | 行为完全一致(全工具只读) |

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
