# Path-scope bug family 修复计划

基线：branch `fix/path-scope-prefix-self`，起点 `a9244b6`（main，与 origin/ddxq 一致）。
基线状态已验证：`cargo check --workspace` 无告警，`cargo test -p veda-core` / `-p veda-sql` 全绿。
所有 file:line 以 `a9244b6` 为准。

结论来源：2026-08-12 全库审计（生产实测复现 #1/#3）+ Codex 对抗性复核（10 条裁决，无一推翻，细节修正已并入本文）
+ 2026-08-12 二次独立核对（逐条 file:line 与行为验证，全部属实；修法修订 3 处，见各条 **[修订]** 标记）
+ 2026-08-12 Codex 第二轮对抗复核（session 019ff59e-a44a-7063-8495-b90681b8ed28）：推翻原 P3 "root 不可达"
  判定（升级为 P1-5），`[` 字母表问题升 P2，其余为措辞/测试修正，已全部并入；明确拒绝项见文末。

## 根因模式（读完再动手）

store 层 `list_dentries_under_page` / `list_dentries_under_capped` 的契约是**目录的严格子孙**：
非根 prefix 拼 `LIKE '{prefix}/%'`（`crates/veda-store/src/mysql/conn.rs:64`），**永远不含 prefix 自身**，
root 走单独分支（conn.rs:49）。"prefix 可能是文件/可能不存在"的判断留给每个 caller 自己做——
search、events 两处做对了（ask 的检索侧对、tool loop 侧断层，见 P1-4），grep、`list_dir_recursive`、
SQL UDTF 忘了。

**正确参考实现（照抄这些形状）：**

- `SearchService::resolve_scope`（`crates/veda-core/src/service/search.rs:158-200`）：先 `get_dentry(prefix)`，
  文件 → 只含该文件；目录 → 子树。注释明文定义语义："a *file* path as prefix means 'search this one file'"。
- SQL 形状：`metadata.rs:590` `query_fs_events` 的 `path = ? OR path LIKE 'prefix/%'`（prefix-self 已含）。
- 边界匹配：`prefix_matches`（search.rs:211），`path == prefix || (starts_with && 下一字节 == b'/')`。

**⚠️ root 陷阱（Codex 复核确认，最容易翻车的点）**：workspace 根**没有 dentry 行**，
`get_dentry(ws, "/")` 返回 `None`。`resolve_scope` 靠上游把 `"/"` 转成 `None`（search.rs:93）才安全。
照抄它的模式时必须先特判 `prefix == "/"` 走原有全量枚举，否则会把默认全库 grep 变成空结果。

**不要做的事**：不要改 `list_dentries_under_page` 的 SQL 语义（比如加 `OR path = ?`）。
严格子孙是被 `resolve_scope` 注释明文依赖的既定契约，改它会波及所有 caller。修点永远在 call site。

---

## P1 必修（用户可达的真 bug）

### 1. grep：精确文件路径返回空

- **现象**：`grep(pattern, "/docs/a.md")` → `LIKE '/docs/a.md/%'` → 静默空结果。生产已实测复现。
  暴露面 4 个：REST `POST /v1/grep`（routes/fs.rs:38）、MCP `grep` 工具 `path` 参数（routes/mcp.rs:747）、
  平台网关 `fs_grep`（routes/project_data.rs:354）、CLI。
- **修点**：`FsService::grep`，`crates/veda-core/src/service/fs.rs:1110-1117`。
- **[修订] 归一化顺手对齐**：fs.rs:1110 现用严格 `normalize`，而 search/events 的 prefix 入口
  全是 `normalize_lenient`。后果：REST/CLI/平台网关传 `docs`（无前导 `/`）→ InvalidPath 400
  （fail-loud，非静默，所以不算 bug）；MCP 面有 `lead_slash`（mcp.rs:747）兜住。
  把这一行换成 `normalize_lenient`，全家统一"宽松收口"语义，一行成本。
  **语义扩展是有意的**（Codex 复核确认无 caller 依赖 400）：`docs`→`/docs` 之外，
  `./docs`→`/docs`、`docs/..`→`/` 也从 400 变为合法（与 search/events 现行完全一致，
  全库 grep 有 50k 枚举上限兜底）。lenient 本身在 path.rs 有单测，grep 侧只加一条接线测试。
- **修法**：normalize 之后（fs.rs:1110 已有）：
  - `prefix == "/"` → 走现有 `list_dentries_under_capped` 全量路径（root 特判，见上）；
  - 否则 `get_dentry(ws, prefix)`：
    - `Some(d)` 且 `!d.is_dir` → 只 scan 这一个文件（复用现有 `read_file_inner` 循环体，binary/missing 同样跳过）；
    - `Some(d)` 且 `d.is_dir` → 现有子树路径；
    - `None` → 返回空（**对齐 search 语义**：`nonexistent_prefix_short_circuits_to_empty`，不报错）。
- **测试**（`crates/veda-core/tests/fs_service_test.rs`，镜像 search_test.rs:459/506/529 的命名风格）：
  `grep_with_file_path_scopes_to_that_single_file` / `grep_with_dir_prefix_unchanged` /
  `grep_root_scans_whole_workspace` / `grep_nonexistent_path_returns_empty` /
  `grep_bare_prefix_equals_slashed`（钉 lenient 接线：`docs` ≡ `/docs`）。
  当前 grep 测试覆盖为零（untested, not masked——mock 忠实复刻严格子孙语义）。
- **收尾**：更新 mcp.rs:413 工具描述（现文案 "Restrict to a path prefix." 与行为矛盾，
  改为 "Restrict to a directory subtree or a single file."）；fs.rs:1094 doc comment 同步。

### 2. list_dir_recursive：文件/不存在路径返回「肯定式空目录」

- **现象**：MCP `list_dir` 带 `recursive: true`（mcp.rs:838）对文件或**不存在的路径**返回
  `{"entries": [], "truncated": false}`，而 mcp.rs:840 注释向调用方承诺 "truncated: false is a fact"。
  非递归 `list_dir`（fs.rs:978-987）对同样输入正确报错 "is not a directory"，两分支自相矛盾。
- **修点**：`FsService::list_dir_recursive`，fs.rs:1061-1074。
- **修法**：进入枚举前补与 `list_dir` 一致的检查：`norm == "/"` 豁免（root 合法但无 dentry）；
  否则 `get_dentry`：`None` → `NotFound`；`!is_dir` → `InvalidPath("… is not a directory")`。
- **连带行为变更（有意为之，需在 commit message 里写明）**：
  - `veda_fs('<file>/')`（SQL DirListing 模式，fs_table.rs:45-50）从返回空变为报错——与非递归 list_dir 对齐；
  - `glob_files`（fs.rs:1078-1092）的 fixed prefix 可能是文件或不存在
    （`glob_fixed_prefix("/notes.md/*")` → `"/notes.md"`，fs.rs:2096）。
    **[修订] 实现形状**：不用 catch `NotFound`/`InvalidPath` 当控制流，改为 `glob_files` 自己
    用**严格 `normalize`**（保持现行入口语义，不顺手放宽——glob 是程序面不是聊天面；
    误用 lenient 会把 `veda_fs('logs/*.txt')` 从报错扩成功）处理 prefix 后分支：
    `"/"` → 直接子树枚举；`get_dentry` → `None` → 空匹配；
    文件 → `glob_match(pattern, path)` 为真则返回该文件否则空；目录 → 现有子树路径。
    **文件分支是有意的行为扩展**（非与 catch 方案等价——catch 形状下字面文件模式返回空，
    新形状返回该文件，消除 fs.rs:2259 单测钉死的 wildcard-free 陷阱；`glob_files` 是 `pub`，
    对新 caller 是活陷阱）。get_dentry 判文件/目录沿用 resolve_scope（search.rs:171）同款语义；
    AI_CI collation 下等价路径多行的二义性是全库 get_dentry 共有的既有已记录问题
    （docs/todos.md），非本修引入，不在本 PR 处理。
- **测试**：`list_dir_recursive_on_file_errors` / `list_dir_recursive_on_missing_path_errors` /
  `list_dir_recursive_root_ok`；glob：`glob_files_literal_pattern_matches_file_itself` /
  `glob_files_missing_prefix_returns_empty` /
  `glob_files_children_pattern_under_file_returns_empty`（`/notes.md/*` → 空，防"任意文件前缀
  都返回该文件"的错误实现假绿）/ `glob_files_root_pattern_ok`（`/*.x`，root 分支不踩陷阱）；
  veda-sql 侧补/调整 `veda_fs('<file>/')` 报错的用例。

### 3. veda_fs_events UDTF：绕过路径归一化

- **现象**：`veda_fs_events(0, 'docs', N)` 静默 0 行，`'/docs'` 正常；REST `/v1/events?path_prefix=docs`
  却能查到。生产已实测复现（'1.DBPaaS' vs '/1.DBPaaS'）。
- **根因**：`fs_events_table.rs:103` 直调 `meta.query_fs_events`，跳过了 choke point
  `FsService::query_events_filtered`（fs.rs:742，做 `normalize_lenient` + `"/"→None`）；
  store 层只去尾部 `/` 不补头部（metadata.rs:583）。
- **修法**：`VedaFsEventsFactory` 改持 `Arc<FsService>`（同模块 `VedaFsTableFactory` 已有先例，
  fs_table.rs:22-24），调用 `query_events_filtered` 走统一收口；调整 SQL engine 的构造处。
- **测试前置**：`MockMetaFull::query_fs_events`（sql_test.rs:172）**完全忽略 `_prefix` 参数**，
  先让 mock 尊重 prefix，语义明写（照 metadata.rs:583-615 生产形状）：
  `None` 或 `"/"` → 全量；非根 → 先 `trim_end_matches('/')`，
  再 `path == prefix || path.starts_with(&format!("{prefix}/"))`。否则新测试测不到真行为。
  现有 UDTF 测试只覆盖 basic/since-id（sql_test.rs:1827 附近），补带/不带前导 `/` 的 prefix
  用例各一；fixture 里加一条 `/docs_alt` 事件，断言 `'/docs'` 不吞它（钉边界语义）。

### 4. answer 的 path_prefix 归一化断层（Codex 复核新发现）

- **现象**：REST `/v1/answer` 把原始 `path_prefix` 传入（routes/answer.rs:131），engine 存原串
  （service/answer.rs:292），检索侧宽松处理（走 search 的 lenient 收口），但 LLM tool loop 里
  `read_file` 的 scope 检查用严格 `normalize`（service/answer.rs:588/597）——
  传 `docs`（无前导 `/`）时检索能命中、文件却读不了（fail-closed，答案质量静默劣化，非越权）。
- **[修订] 暴露面比原文多两个，修点上移到 engine**。`svc.answer` / `svc.answer_stream` 共有
  三个**生产**入口（另有 core 单测直调，answer.rs:1136/1371/1389/1515，随修随过）：
  REST 非流式（routes/answer.rs:128）、REST 流式（routes/answer.rs:249，**原文漏了这个，同样坏**）、
  MCP `ask`（mcp.rs:945，今天靠 `lead_slash`（mcp.rs:924）兜住前导斜杠所以基本没坏）。
  engine 内 `answer` 自己调 `answer_stream`（service/answer.rs:245），
  所以 **`AnswerService::answer_stream` 入口（service/answer.rs:267）是唯一收口点**——
  在那里做 `normalize_lenient` + `"/"→None`（照抄 search.rs:93-104 的形状），
  一处覆盖全部三个面；原文"在 routes/answer.rs 入口修"会重演本计划批判的
  "修复半径漏 sibling caller"。routes 与 mcp.rs 无需改动（mcp 的 `lead_slash` 变冗余但无害，留着）。
  错误类型已兼容：`AnswerError::Store(VedaError)`（answer.rs:118）+ 既有 `From` 转换（answer.rs:281 `r?` 先例），
  `normalize_lenient` 失败 → `Store(InvalidPath)` → HTTP 400 / MCP ToolError，与 events 路由 fail-loud 姿态一致，零新增 plumbing。
- **验收**：REST answer（流式+非流式）与 MCP ask 传 `docs` 与 `/docs` 行为一致。
- **测试**：现有 `path_prefix_blocks_out_of_scope_read`（answer.rs:1396）只测 `/docs`；
  加一条裸前缀自动化回归：`path_prefix="docs"` 时 tool loop 能读 `/docs/...`（钉本 bug 本体，
  不再只靠手工验收）。

### 5. root destination guard（Codex 第二轮复核推翻原 P3 前提，升级新增）

- **现象**：原 P3 判定"root 无 dentry ⇒ 陷阱不可达"的前提**锁不住**：
  `reject_reserved_basename("/")` 放行（`filename("/") == ""` 不在保留名单，path.rs:155-164），
  rename 的 `dst_exists` 检查因 root 无 dentry 恒为 `None`（fs.rs:1653）。于是公开面就能造出
  root dentry：REST `fs-rename {from:"/dir", to:"/"}` → dentry 改写为 `path="/"`、子项变 `//child`
  （数据损坏）；`fs-copy to:"/"`、`write_file("/")`（平台上传/SQL `veda_write` 可达）、
  `append_file("/")` 同坑。`normalize("") == "/"`（path.rs:173 单测钉死），空串目标同样中招。
  root dentry 一旦存在，原 P3 全族陷阱（tx.rs:158、fs.rs:1659/1695）立即激活。
- **修法（修根因，不在陷阱处撒 guard）**：`reject_reserved_basename` 扩展语义为
  "**目标必须有非空 basename**"——`filename(path).is_empty()`（即 path == "/"）→ InvalidPath
  （"root cannot be a write/copy/rename destination"）。它的 6 个 caller 全是目标校验
  （write_file:348/419、append_file:1481、mkdir:1307、copy dst:1355、rename dst:1638），一处收口。
  唯一语义豁免：mkdir 把现有 `norm == "/" → Ok(())`（fs.rs:1308，幂等语义）**提到校验之前**，
  两行换序。锁死"root 无 dentry"不变量后，P3 判定重新成立，五处休眠代码继续休眠。
- **测试**：`rename_to_root_rejected` / `copy_to_root_rejected` / `write_to_root_rejected`
  （空串目标走同一路径，`mkdir("/")` 仍幂等 Ok）。

## P2 顺手修（低成本，同 PR 带上）

6. **删除死字段 `SearchRequest.path_prefix`**（types.rs:534；赋值点 search.rs:255/358）。
   无任何 store 消费（Milvus 只吃 `id_filter`，milvus.rs:1255）；无跨进程 wire 波及
   （公开 REST/平台用独立的 `SearchApiRequest`，api.rs:183；CLI/tunnel 手写 HTTP）。
   留着的风险：将来某个 store 实现它时写个裸 `LIKE 'prefix%'`，在一个长得像权限过滤的 API 下
   重新引入边界 bug。无需向后兼容，直接删。编译修改点（Codex 清点，删字段后 cargo check 兜底）：
   search_table.rs:160-169、types_test.rs:193-201、search_test.rs:500、
   milvus_test.rs:80/100/714/932/1005。
7. **trait 文档补语义**（store.rs:91）：`list_dentries_under_page` 明写三条 caller 实际依赖的语义——
   严格子孙 / prefix 自身排除 / root 单独分支。
8. **`[` 字母表统一**（Codex 第二轮从 P3 升级：非纯性能——fixed prefix 遇 `[` 回退 root 后
   撞 10k glob cap，大 workspace 里字面 `[` 模式从能成功变 `QuotaExceeded`）：
   删 fs.rs:2099 的 `|| part.contains('[')` 一行，字母表与 matcher（只实现 `*`/`?`/`**`）
   和 `detect_mode`（fs_table.rs:94，只认 `*`/`?`）对齐；P1-2 的文件分支顺带让字面 `[` 路径
   走直达。现有 `test_glob_fixed_prefix` 无 `[` 用例，无需调整。
9. **修正 stale 的 ignored E2E 断言**（remote_e2e_test.rs:83-94/898-906）：断言裸前缀 events 400，
   但 events 路由已 lenient（events.rs:96，先于本 PR），一行翻转成断言等价。

## P3 不修，仅记录（休眠于「root 无 dentry」不变量之后，由 P1-5 guard 锁死）

以下同族问题在 P1-5 落地后不可达（rename **source** 为 `"/"` 本就死于 fs.rs:1648 的 `get_dentry`，
destination 侧由 P1-5 拒绝），按反过度设计原则不动代码，在 `docs/todos.md` 记一条
（触发条件：若未来有意给 root 建 dentry 行，以下全部激活）：

- `rename_dentries_under` 缺 root 分支（tx.rs:158-160，`"/"` → `LIKE '//%'`；隔壁 delete tx.rs:108 有）；
- 目录自嵌检查对 root 构造 `"//"`（fs.rs:1659）；child Move event rebase 对 root 丢分隔符（fs.rs:1695）;
- 三个 tx mock 的 root 分支与生产不一致（mock_store.rs:630/642、sql_test.rs:325/347）。

FUSE `InodeTable::rename_path`（inode.rs:110）经复核**不是问题**：FUSE 协议层不可能以挂载根为
rename 对象（veda-fuse fs.rs:254/1887），无需处理。

## 统一验收清单

```sh
cargo test -p veda-core -q && cargo test -p veda-sql -q     # 全绿

# 行为翻转（前两条修复前已在生产 daldocs workspace 复现为坏行为）
veda grep "<pattern>" /path/to/existing/file.md              # → 命中（修前：空）
veda sql "SELECT * FROM veda_fs_events(0,'1.DBPaaS',3)"      # → 与 '/1.DBPaaS' 等价（修前：0 行）
# MCP list_dir {path:"<file>", recursive:true}               # → "is not a directory"（修前：空+truncated:false）
veda sql "SELECT * FROM veda_fs('<file>/')"                  # → 报错（修前：空）※有意的行为变更

# P1-4（修订后半径）
# REST /v1/answer 与 /v1/answer/stream 传 path_prefix: "docs" ≡ "/docs"
# MCP ask 传 path_prefix: "docs" ≡ "/docs"（原有 lead_slash 行为不回退）

# P1-5 root guard
# REST fs-rename {from:"/dir", to:"/"} → 400（修前：造出 root dentry + //child 数据损坏）
# REST fs-copy to:"/" / 写入 path:"/" / 空串目标 → 400；mkdir "/" 仍幂等 Ok

# 回归护栏（昨天 7c15fe6 的修复不能被破坏）
veda grep "<pattern>" /some/dir                              # 目录语义不变
veda grep "<pattern>"                                        # 全库语义不变
veda search "<q>" --path /path/to/file.md                    # 文件路径 scope 仍工作
```

## Codex 第二轮复核后明确不做的（拒绝理由存档）

- **`.`/`./docs`/`docs/..` 的逐 surface 测试矩阵**：lenient 折叠语义在 path.rs 有单测，
  每个 surface 一条裸前缀接线测试足够；语义扩展本身是改动目的。
- **`glob_files` 的 `max_matches == 0` 分支**：无任何 caller 传 0，理论输入不写分支。
- **真 MySQL collation 测试设施**：AI_CI 等价路径二义性是全库 get_dentry 共有的既有已记录问题
  （docs/todos.md），resolve_scope 参考实现同样暴露，非本 PR 引入；为它建 MySQL 测试基建属过度。
- **升级修复全部 P3 root 陷阱**：P1-5 在信任边界一处锁死不变量后，五处休眠代码继续休眠；
  给"不该发生的操作"写支持代码才是过度防御。

## 溯源（一句话版）

严格子孙语义 2026-04 随 store 层诞生（当时 caller 全是目录输入，语义正确）；05-07 grep 复用它接
用户输入，bug 潜伏；07-14 ask、08-11 search（7c15fe6）先后各自局部修对，语义决策
（"file path as prefix scopes to that single file"）在 7c15fe6 的 commit message 里首次明文化，
但修复半径未覆盖 sibling caller——本计划补齐剩余部分。
