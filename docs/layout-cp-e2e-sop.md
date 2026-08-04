# workspace layout + cp ignore — 测试环境手动 E2E SOP

> 适用版本：CLI **≥ 0.1.24**（layout 渲染修复 + `.git` 指针文件跳过在这版），server **≥ 0.1.23**（`/v1/layout` 与 MCP `layout` 在这版定名），部署于测试节点 .161/.89。
> 预计耗时：15–20 分钟。每步都有「预期」，不符即停，按文末排障查。
> 覆盖：`veda cp` 的 .gitignore/.vedaignore 语义（c0b7a6a + d298809）、`veda layout` / `GET /v1/layout` / MCP `layout`（a04d278 + 1be8c34）、CLI 渲染修复的实景验证（bb7792e）。

## 0. 前置

- **mac 直连测试数据面**：`https://veda.dbpaas.dingdongxiaoqu.com`（不是网关域名 `paas-api-test...`，那个只认 passport 登录态，`wk_` 会 401）。终端清代理（大小写都要，reqwest 两种都读）：`unset http_proxy https_proxy all_proxy HTTP_PROXY HTTPS_PROXY ALL_PROXY`，Clash 走 TUN。
- **先隔离、再装 CLI**（顺序重要：install.sh 会往 `${XDG_CONFIG_HOME:-~/.config}` 写 server_url，且默认值是**生产** URL——先设好隔离目录，它写进的就是马上会被覆盖的一次性配置，不碰你的真实环境）：

```bash
export XDG_CONFIG_HOME=$(mktemp -d)
export BASE=https://veda.dbpaas.dingdongxiaoqu.com
curl -fsSL "$BASE/install.sh" -o /tmp/veda-install.sh
VEDA_INSTALL_DIR=/tmp/veda-sop-bin sh /tmp/veda-install.sh   # binary 进临时目录，不覆盖已装的 veda
export PATH=/tmp/veda-sop-bin:$PATH && hash -r
veda --version   # 预期 0.1.24；不对先查 which veda（PATH 里有旧 binary）
```

> ⚠️ install.sh 有一个 `VEDA_INSTALL_DIR` 管不住的副作用：机器上存在 `~/.claude` 时会**无条件覆写** `~/.claude/skills/veda/SKILL.md`。自己改过这份 skill 的先备份。
- **干净 workspace**（layout 是整个 workspace 根级视图，混着旧内容会没法精确断言）：

```bash
# 有账号 key（vk_）：导入后另开一个专用 workspace
veda init --server "$BASE" --import-key "vk_..."
veda workspace add sop-e2e && veda workspace use sop-e2e
veda status
```

> 只有 `wk_` 时改用 `veda init --server "$BASE" --import-key "wk_..."` 接既有 workspace。此路径有**数据风险**，先做冲突预检：
>
> ```bash
> veda ls /   # 确认根下不存在这 8 个名字：.github sub worktree 文档中心 .gitignore .vedaignore README.md keep.log
> ```
>
> 因为 `veda cp` 对同路径是**静默覆盖**（写新 revision，不提示），第 12 步的 `rm` 又会把这些路径整个删掉——重名时你的原件会被先盖后删。有任何一个重名就换专用 workspace，别硬跑。断言方面：条数/表尾统计会混入既有内容，差值应恰好等于本 SOP 的 10 files / 5 directories / 顶层 +8 条目，属预期。

## 1. 构造样本树

一棵树同时踩中所有 ignore 语义 + CJK 目录名：

```bash
SOP=/tmp/veda-sop-src && rm -rf $SOP && mkdir -p $SOP && cd $SOP
SENTINEL="LAYOUT_SOP_$(date +%s)"; echo "哨兵词：$SENTINEL"

printf 'target/\n*.log\n'      > .gitignore
printf '!keep.log\nsecret/\n'  > .vedaignore
printf '%s\nveda layout E2E 的入口文档。\n' "$SENTINEL" > README.md
echo kept    > keep.log                                  # *.log 被 .vedaignore 否定规则救回
echo noise   > debug.log                                 # *.log → 跳过
mkdir -p target/debug   && echo junk > target/debug/build.out    # target/ → 整树剪枝
mkdir -p secret         && echo tok  > secret/token.txt          # 仅 .vedaignore 规则 → 跳过
mkdir -p 文档中心        && echo 规范正文 > 文档中心/规范.md && echo 流程正文 > 文档中心/流程.md
mkdir -p sub            && printf '*.tmp\n' > sub/.gitignore
echo tmp     > sub/cache.tmp                             # 嵌套规则 → 跳过
echo note    > sub/note.md
mkdir -p .github/workflows && echo 'on: push' > .github/workflows/ci.yml   # dotfile 是真内容
mkdir -p .git           && echo cfg  > .git/config               # .git 目录 → 内置剪枝
mkdir -p node_modules/pkg && echo js > node_modules/pkg/index.js # 内置剪枝
touch .DS_Store                                                  # 内置文件名单
mkdir -p worktree && printf 'gitdir: /repo/.git/worktrees/x\n' > worktree/.git   # 0.1.24 新修：.git 指针文件
echo wt      > worktree/wt.md
ln -s /etc/hosts link_out                                        # symlink → 跳过并提示
```

**应上传的恰好 10 个**：`.gitignore` `.vedaignore` `README.md` `keep.log` `文档中心/规范.md` `文档中心/流程.md` `sub/.gitignore` `sub/note.md` `.github/workflows/ci.yml` `worktree/wt.md`。
其余 9 项（debug.log、target/、secret/、sub/cache.tmp、.git/、node_modules/、.DS_Store、worktree/.git、link_out）全部跳过。

## 2. 上传：ignore 规则生效（新能力①）

```bash
veda cp $SOP /
```

**预期**：
- stderr 一行提示 `(10 files to upload; .gitignore/.vedaignore rules applied — --no-ignore to upload everything)`；
- stderr 一行 `skip symlink: .../link_out`；
- 逐行列出且**只有**上面 10 个文件；结尾 `Uploaded 10 file(s) under /`，可能附一行 indexing 排队提示。

## 3. 远端核对：跳过的确实没上去

```bash
veda ls /            # 预期目录：.github/ sub/ worktree/ 文档中心/；文件：.gitignore .vedaignore README.md keep.log
                     # 不得出现：debug.log、target、secret、node_modules、.git、.DS_Store
veda ls /worktree    # 预期只有 wt.md（.git 指针文件被跳过——0.1.24 修复点）
```

## 4. `--no-ignore` 逃生舱（关规则、保内置剪枝）

```bash
veda cp --no-ignore $SOP /noignore
```

**预期**：**14** 个文件（原 10 + debug.log + target/debug/build.out + secret/token.txt + sub/cache.tmp）；`.git/`、`node_modules/`、`.DS_Store`、`worktree/.git` 依然跳过；**没有** rules applied 提示行。核对后清掉：`veda rm /noignore`。

## 5. 坏 ignore 文件必须响亮失败（不能静默传爆）

```bash
cp -R $SOP /tmp/veda-sop-bad && printf '[z-a]\n' >> /tmp/veda-sop-bad/.gitignore
veda cp /tmp/veda-sop-bad /bad
```

**预期**：直接报错 `ignore rules under ... could not be parsed`，退出码非 0，**0 个文件上传**（walk 阶段失败，不会传一半）。`rm -rf /tmp/veda-sop-bad`。

## 6. 等索引与摘要

```bash
veda status --index --wait     # 等 pending/processing 清零
```

L0 摘要由 LLM worker 异步生成，比索引慢一拍（通常 1 分钟内，最长 3 分钟）。期间 `veda layout` 显示 partial 脚注属正常，消失即就绪。

## 7. `veda layout` 人类可读输出（新能力② + 渲染修复实景）

```bash
veda layout
```

**预期**（形状示意：摘要文案每次不同，第二列**右对齐**，列宽随最宽单元格浮动，以「同列起始格一致」为准，不逐字符比对）：

```
.github/     1 file   <L0 摘要>
sub/         2 files  <L0 摘要>
worktree/    1 file   <L0 摘要>
文档中心/     2 files  <L0 摘要>
.gitignore     <N> B  <L0 摘要>
.vedaignore    <N> B  <L0 摘要>
README.md      <N> B  <L0 摘要>
keep.log       <N> B  <L0 摘要>

10 files, 5 directories, <N> KB
```

逐项核对：
- **目录在前、组内按字节序**（`文档中心/` 排目录组末尾属正常：CJK 字节序在 ASCII 后）；
- **CJK 对齐**（bb7792e 修复点）：`文档中心/` 行的第二、三列与其他行**同列起始**，肉眼平齐；
- `文档中心/` 计数为 **2 files**（递归计数；旧 `ls | wc -l` 拿不到这个数）；
- 表尾 `10 files, 5 directories`（5 = `.github`、`.github/workflows`、`sub`、`worktree`、`文档中心`——嵌套目录也计入，workspace 根没有 dentry 不算）；
- 摘要全部就位后**没有任何括号脚注**；还在生成时允许出现 `(some summaries are still being generated...)`。

> 渲染的边角（换行伪造行、负数、超长截断、字节进位）由 CLI 单测守护（159 条），SOP 只做实景 CJK 对齐与整体形状目检。

## 8. `veda layout --json`（agent 走的那条路）

```bash
veda layout --json | jq '{state: .summary_state, trunc: .truncated, n: (.entries|length), files: .stats.total_files}'
# 预期：{"state":"ready","trunc":false,"n":8,"files":10}   （state 未就绪时为 "partial"）
veda layout --json | jq '[.entries[] | select(.is_dir) | has("file_count") and (has("size_bytes")|not)] | all'    # true
veda layout --json | jq '[.entries[] | select(.is_dir|not) | has("size_bytes") and (has("file_count")|not)] | all' # true
```

## 9. REST 直连（网关/SDK 消费的形状）

```bash
WK=$(grep -o 'wk_[A-Za-z0-9]*' "$XDG_CONFIG_HOME/veda/config.toml" | head -1)   # 或直接用手里的 wk_
curl -s -H "Authorization: Bearer $WK" "$BASE/v1/layout" | jq '.success, (.data.entries|length)'
# 预期：true / 8
```

## 10. MCP `layout` 工具（Coding Agent 走的那条路）

```bash
curl -s -H "Authorization: Bearer $WK" -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' "$BASE/mcp" \
  | jq '(.result.tools|length), .result.tools[0].name'
# 预期：7 / "layout"（排第一是刻意的——initialize instructions 引导首调）

curl -s -H "Authorization: Bearer $WK" -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"layout","arguments":{}}}' "$BASE/mcp" \
  | jq -r '.result.content[0].text' | jq '.summary_state, (.entries|length)'
# 预期：与第 8 步同源同值（REST 与 MCP 共用 build_workspace_layout）
```

## 11. 检索连通（上传内容真的进了索引）

```bash
veda search "$SENTINEL"      # 预期命中 /README.md
```

## 12. 清理

```bash
veda rm /文档中心 /sub /worktree /.github /.gitignore /.vedaignore /README.md /keep.log
rm -rf $SOP; unset XDG_CONFIG_HOME BASE   # 或直接关掉这个终端
```

> `rm` 的确认行为：TTY 下先问一次 `y/N`；非 TTY（脚本/CI）**不询问直接执行**，只在 stderr 公告——脚本里删错路径没有后悔药，`wk_` 共享 workspace 路径尤其核对清楚再回车。

（专用 workspace 留在测试环境无妨；要删走 console/admin。）

## 排障

| 症状 | 先查 |
| --- | --- |
| 步步 5xx / 超时 | `curl -s $BASE/healthz` ——先分清「测代码 vs 环境挂了」，测试节点被拖垮时 e2e 全 502 会误判成代码问题 |
| `veda layout` 报 404 / unknown route | server 还在 0.1.22（路由还叫 `/v1/map`）。一次定死代次：`curl -o /dev/null -sw '%{http_code}' -H "Authorization: Bearer $WK" $BASE/v1/map` 与 `.../v1/layout`——`404/200`=新码（对），`200/404`=旧码（先查发布） |
| 401 | 用了网关域名（只认 passport），或 key 贴错；必须直连 `veda.dbpaas.dingdongxiaoqu.com` |
| 连不上 / DNS 诡异 | 代理没清干净（`env \| grep -i proxy`）；Clash 需 TUN 模式（fake-ip 走 198.18.x.x 正常） |
| layout 一直 partial | `veda status --index` 看 pending 是否清零；再等 1–2 分钟 L0 批次；`disabled` 脚注 = server 没配 `[llm]`，是配置不是 bug |
| `--version` 不是 0.1.24 | `which veda`——PATH 里残留旧 binary，重开终端或删旧的 |
| cp 上传数对不上 | 先跑 `veda cp` 看逐行清单，比对第 1 步的 10 个名单，定位哪条规则没生效 |
