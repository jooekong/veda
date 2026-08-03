# `veda cp` ignore 规则 + workspace 级 map

> 状态：待实施（已过 codex 方案评审，v2）
> 作者：Claude (Opus 5)
> 日期：2026-08-03
> 来源：对标 `Graphify-Labs/graphify` 后拍板的两条改进
>
> **v5 改名（实施后）**：`map` → **`layout`**，三处统一（REST `GET /v1/layout`、MCP 工具
> `layout`、CLI `veda layout`）。理由：① `map` 在存储语境里是动词=挂载（Windows "map network
> drive"），而 veda 恰好有 FUSE 挂载，`veda map` 会被误读；② 本文档早期考虑过的 `outline` 与既有
> 的 `abstract`/`overview` 是英语近义词，三者并列无法区分。`layout` 表达的是「作用域/形态」而非
> 「摘要深度」，且**不承诺完整性**——这点很重要，本端点截断在 200 条且是 best-effort 非快照，
> `inventory`/`catalog`/`manifest` 那类词会过度承诺。改名在上测试环境当天完成，彼时零外部消费者。
> 同时补上 **CLI `veda layout`**（原 D14 决定不做，后被推翻：`skill.md` 明确 coding agent 经
> `veda` 二进制交互，CLI 是没配 MCP 时的默认路径，token 经济学在该路径上完全成立）。
>
> **v4 实施记录**（需求二实现 + codex 代码评审后）：
> - **发现一个既有的系统属性，不是本次引入，但影响 map 的正确设计**：`veda_dentries` 的
>   bootstrap DDL **没有指定 `CHARACTER SET` / `COLLATE`**，`path` 继承建库默认值；测试库上
>   是 `utf8mb4_0900_ai_ci`（大小写 + 重音都不敏感）。而 `get_dentry` / `list_dentries` 都是
>   `WHERE path = ?` / `parent_path = ?` 直接比较该列 —— 所以 **veda 的路径查找本来就是
>   大小写不敏感的**：写 `/Docs/a.md` 再写 `/docs/b.md`，第二次 `ensure_parents` 会复用
>   `/Docs`，不会创建第二个目录 dentry，而两个文件因 `path_hash = SHA2(path)` 各自存在。
>   实测确认（`file_counts_follow_the_case_insensitive_path_semantics`）。
>   → **`file_count` 因此必须沿用同一套折叠语义**：一度加了 `COLLATE utf8mb4_bin` 想「修正」
>   合并，结果反而让 map 与它所描述的 `list_dir` 互相矛盾，已回退。Rust 侧用
>   `fold_path_segment`（NFD 去组合符 + 小写）近似对齐 MySQL 的分组键。
>   → **遗留待决**：DDL 不固定 collation 意味着**路径大小写敏感性取决于建库时的默认值**，
>   不同部署可能行为不同。固定它需要对生产表做迁移且会改变路径语义，超出本方案范围，
>   单独立项。
> - `ORDER BY ... path COLLATE utf8mb4_bin` **保留**：只是给 ai_ci 下相等的项加确定性排序，
>   不改匹配语义，避免截断边界处返回结果在多次调用间漂移。
> - **集成测试之间会通过共享 MySQL 互相污染**：`hard_delete_workspace` 只删
>   `veda_workspaces` 一行，dentry / file / **outbox** 全留。map 测试原本用 250 个文件测截断，
>   排下约 500 个 embedding 任务，把随后运行的 `mcp_http_test` 的 120s 预算吃光导致假红。
>   改为用 `mkdir` 造目录（不排队任何任务）+ cleanup 补全 file/content/outbox 清理。
>   顺带把 mcp 集成测试从 113s 降到 8s（它此前一直在替历史积压打工）。
>
> **v3 修订**（需求一实现后的 codex 代码评审 + 源码复核）——三条 v2 未预见的真问题：
> - **`parents(false)` 并没有阻止读取祖先 `.gitignore`**。`Ignore::add_parents`（`dir.rs:196`）只在
>   `parents`/`git_ignore`/`git_exclude`/`git_global` 四个**全 false** 时才短路。祖先规则不参与匹配
>   （`matched_ignore` 在 `is_absolute_parent` 处 `take_while` 停止，所以 v2 的测试通过），但**照样被解析**，
>   畸形 glob 会冒泡成 walk error 中止上传。改为 `git_ignore(false)` +
>   `add_custom_ignore_filename(".gitignore")`（先）与 `.vedaignore`（后，故可覆盖）。`require_git` 随之无关，删除
> - **ignore 文件的解析错误挂在成功的 `DirEntry` 上**（`DirEntry::error()`），不会让 walk 失败。
>   吞掉它 = `.gitignore` 打个错字就静默上传整个 `target/`。改为 fatal
> - `has_ignore_file` 只查源根，但嵌套 `.gitignore` 也生效 → 提示会漏打。改为 `collect_files` 回报 `rules_seen`
>
> **v2 修订**（codex max-effort 方案评审 + 独立复核后）：
> - 修正 `ApiResponse` 形状、`WORKSPACE_KIND_MISMATCH` 大小写、`veda_dentries` 索引三处事实错误
> - 修 `collect_files` 骨架的真 bug：兜底列表必须用 `filter_entry` 下降前剪枝，否则整个 `.git/` 会被上传
> - ignore 来源收敛为「只认源目录树内的 `.gitignore` / `.vedaignore`」（关 `parents` / `.ignore` / `git_global` / `git_exclude`）
> - map 的规模上限前推到 store 层（原方案只截响应不截读取）
> - **删除原 D16**（改 `list_child_summaries` 签名）——改用「先取 ≤cap 个 dentry，再按 id 批量取 summary」，worker 与 3 处 mock 全部不动
> - 补 `MapSummaryState` 定义，重新定死 `disabled` 语义
> - 重写需求二 DoD（原方案在现有 mock / `mcp_http_test` 下跑不出来）

---

## 0. 背景与总目标

两条独立改进，按顺序做，需求一先。

| # | 需求 | 一句话 |
| --- | --- | --- |
| 一 | `veda cp` 尊重 `.gitignore` / `.vedaignore` | 传仓库不再把 `target/` 灌进知识库烧 embedding + LLM 配额 |
| 二 | `GET /v1/layout` + MCP `layout` 工具 | 给"这个知识库整体是什么"一个入口，**零新增 LLM 调用** |

---

## 1. 施工前先纠正的两处事实错误

调研原始需求描述时发现两处与代码不符，方案按纠正后的事实设计。

### 1.1 workspace 根目录**没有** L1 overview（影响需求二核心设计）

原始设想是"map 返回根目录 L1 overview + 二级目录列表"。**根目录 L1 不存在**。

`crates/veda-server/src/routes/search.rs:16-22` 有明确注释：

```rust
// NOTE: there is intentionally no bare `/v1/abstract` /
// `/v1/overview` route for the workspace root. The summary
// service resolves a row by dentry, and the root path has no
// dentry — adding the route just produced misleading 404s
// (caught by the 2026-05-14 adversarial review). When root-level
// summaries land as a real feature (worker + store), wire them
// here.
```

`SearchService::get_summary`（`veda-core/src/service/search.rs:197`）先 `get_dentry(ws, path)`，根路径 `/` 查不到 dentry → 直接 `Err(NotFound)`。`veda_summaries` 表的行要么挂 `file_id` 要么挂 `dentry_id`，根目录两者都没有。

**结论**：map **不能**依赖根 L1。反过来说——**map 本身就是根级视图**，用顶层条目的 L0 确定性组装出来，正好是"数据现成、只做组装、零 LLM"的要求。这比生成一个根 L1 更省（不用动 worker/store 去造根 summary 行），也绕开了 05-14 评审踩过的坑。

### 1.2 `list_child_summaries` 不返回 path，且只返回 ready 行

`crates/veda-store/src/mysql.rs:1487` 的实现 `SELECT` 里没有 `d.path`，`FileSummary`（`veda-types/src/types.rs:508`）也没有 path 字段——只有 `file_id` / `dentry_id`。现有唯一调用方是 worker 的目录聚合（`worker.rs:635`），它只要 L0 正文，不需要 path。

map 需要把 abstract 对齐到具体路径，所以这个方法**必须改**（§3.4）。

WHERE 里带 `s.status = 'ready'`，pending 的行不会返回——这是对的，map 不该展示半成品摘要。

---

# 需求一：`veda cp` 支持 `.gitignore` / `.vedaignore`

## 2.1 现状

`crates/veda-cli/src/main.rs`：

```rust
// :1943
const IGNORED_DIRS: &[&str] = &[".git", "__pycache__", ".idea", "node_modules"];
const IGNORED_FILES: &[&str] = &[".DS_Store"];
```

`collect_files()`（:1947）用 `std::fs::read_dir` 手写递归，`cp_dir_recursive()`（:1886）调它并打印跳过计数。

问题：`target/`、`dist/`、`build/`、`.venv/`、`vendor/` 全不在列表里。传一个 Rust 仓库把整个 `target/` 灌进去，**每个文件触发 embedding + L0 + L1 三次下游调用**。

## 2.2 依赖

`ignore` crate（ripgrep 同款），**当前不在 Cargo.lock**，需新增到 `crates/veda-cli/Cargo.toml`：

```toml
ignore = "0.4"
```

不进 `[workspace.dependencies]`——只有 CLI 用，其他 crate 没有需求，放 workspace 级是无谓的扩散。

传递依赖 `globset` / `walkdir` / `same-file` / `crossbeam-*`，其中 `walkdir` / `same-file` 已在 lockfile（datafusion 侧拉进来的）。CLI 是发版二进制，体积增量可接受（约 +200KB）。

## 2.3 设计决策（逐条定死）

### D1. 兜底列表：**保留，与 gitignore 规则合并**

不替换。理由：

- `.git/` 永远不会写进 `.gitignore`（git 不需要忽略自己），但它绝对不该上传
- `.DS_Store` 通常在用户的**全局** gitignore 里，而非仓库 `.gitignore`
- 源目录可能根本没有 `.gitignore`（非 git 目录），此时兜底列表是唯一防线

实现：`ignore::WalkBuilder` 的 filter 之上再叠一层现有的名字匹配。保留 `IGNORED_DIRS` / `IGNORED_FILES` 两个常量不动。

**不做会怎样**：传非 git 目录时退化回"全传"，`.git` 也会被传（如果目录是 git 仓库但用了 `--no-ignore`）。

### D2. `require_git(false)` —— 必须显式设

`ignore` crate 默认 `require_git(true)`：**不在 git 仓库里时，`.gitignore` 文件被完全忽略**。

veda 场景下用户很可能在一个非 git 的文档目录里放 `.gitignore` 或 `.vedaignore` 来控制上传。默认值会让这些文件静默失效。

```rust
.require_git(false)
```

**不做会怎样**：非 git 目录里的 `.vedaignore` 不生效，且不报错——最坏的一类 bug。

### D3. `hidden(false)` —— 必须显式设（最容易踩的一条）

`ignore` crate 默认 `hidden(true)`：**跳过所有 `.` 开头的文件和目录**。

现状是**传所有 dotfile**（只排除 `.DS_Store`）。用默认值会静默漏传：

- `.github/workflows/*.yml`
- `.env.example`
- `.claude/`、`.cursor/rules/`
- `docs/.vitepress/`
- `.eslintrc`、`.prettierrc` 等配置

对知识库场景这是**功能回归**，且用户完全不会察觉（skipped 计数只给一个数字）。

```rust
.hidden(false)
```

`.git/` 由 D1 的兜底列表挡掉。

**不做会怎样**：用户传完发现 `.github/` 不见了，且不知道为什么。

### D4. ignore 来源只留两个：`parents(false)` + 关掉 `.ignore` / `git_global` / `git_exclude`

**v1 原设计（保持 `parents(true)` 与全部 git 来源默认开启）已推翻。**

推翻理由（codex 评审 + 复核）：

1. `require_git(false)`（D2 必需）叠加 `parents(true)` 后，walker 会向上读到 **git root 之上**的 `.gitignore`——`ignore` crate 文档明确说明这**不同于 git 行为**。v1 里"与 git 行为一致"的说法是错的。一个躺在 `~/` 的 `.gitignore` 会静默影响上传结果。
2. `git_global`（`~/.config/git/ignore`）与 `git_exclude`（`.git/info/exclude`）让**同一目录在不同机器 / 不同用户下传出不同内容**。知识库内容不该受本机 git 配置暗中摆布。
3. `.ignore` 文件是 ripgrep 约定且**优先级高于 `.gitignore`**，v1 说"保持开启无害"是错的——用户为 ripgrep 配的 `.ignore` 会意外改变上传结果。
4. 保留 `git_global` 的唯一论据是"用户的 `.DS_Store` 全局规则"，但 `.DS_Store` 已被 D1 的兜底列表覆盖，论据不成立。

**最终语义（一句话）**：只认**源目录树内**的 `.gitignore` 和 `.vedaignore`，外加内置兜底列表。

```rust
.parents(false)      // 不向上越界
.ignore(false)       // 不读 .ignore（ripgrep 约定，优先级高于 .gitignore）
.git_global(false)   // 不读 ~/.config/git/ignore
.git_exclude(false)  // 不读 .git/info/exclude
.git_ignore(true)    // 只留这个
.add_custom_ignore_filename(".vedaignore")
```

代价：`veda cp ./subdir` 不再吃仓库根的 `.gitignore`。**接受**——"我上传这个目录，规则就来自这个目录"更符合直觉，且行为可一句话讲清、可测。真需要仓库根规则的用户在子目录放一个 `.vedaignore` 即可。

**不做会怎样**：上传结果取决于用户 home 目录里有什么、本机 git 怎么配的，跨机器不可复现，且用户无从察觉。

### D5. symlink：保持现有行为并保留注释

`WalkBuilder` 默认 `follow_links(false)`，遍历时 symlink 作为条目返回但不跟随。现有代码的显式跳过 + `eprintln!("skip symlink: ...")` 提示要保留，连同那段解释"防无限递归 + 防越界上传"的注释一起搬过去。

源目录 root 本身是 symlink 的情况不变：`cp` 处的 `src_path.is_dir()` 跟随 symlink，仍会进入递归。

### D6. 逃生舱：`--no-ignore`

```rust
Cp {
    src: String,
    dst: String,
    /// Upload everything, ignoring .gitignore / .vedaignore rules.
    /// The built-in skip list (.git, node_modules, .DS_Store, ...) still applies.
    #[arg(long)]
    no_ignore: bool,
},
```

单个 bool。**不做** ripgrep 那套 `--no-ignore-vcs` / `--unrestricted` 分级——veda 用不上，是纯复杂度。

`--no-ignore` 下兜底列表**仍然生效**（`.git/` 无论如何不传）。

### D7. 用户可见反馈：**不承诺精确总数**

现状文案把列表写死了：

```rust
eprintln!("  (skipped {ignored} ignored entr{}: {}, {})",
    ..., IGNORED_DIRS.join("/"), IGNORED_FILES.join("/"));
```

**v1 原设计（报一个精确的 skipped 总数）不可实现。** 被 gitignore 规则剪掉的条目**根本不会进入 walker 的迭代器**，`ignore` crate 也不暴露"因何被忽略"的钩子——拿不到那个数。v1 承诺 `skipped 1523 entries` 是在编一个算不出来的数字。

改为只报能诚实拿到的信息：

```
  145 files to upload (.gitignore/.vedaignore rules applied; --no-ignore to include everything)
```

- 上传数 = `files.len()`，确定可得
- 括号里的提示只在**非 `--no-ignore` 且源树里确实存在 ignore 文件**时打印，否则纯噪音
- 兜底列表剪掉的目录数在 `filter_entry` 里可见，但**单独报这个数没有行动价值**（用户不关心跳了几个 `.git`），不报

**不做会怎样**：打一个编造的数字，用户拿它去核对"是不是漏传了"会得出错误结论。

### D8. 遍历顺序：`sort_by_file_path()`

`ignore::Walk` 不保证顺序（现有 `read_dir` 也不保证）。为了输出可读、测试可断言，用 `WalkBuilder::sort_by_file_path()`。单线程 `build()`（不是 `build_parallel()`）——上传本身是串行 await，并行遍历没有收益。

## 2.4 改动清单

| 文件 | 改动 |
| --- | --- |
| `crates/veda-cli/Cargo.toml` | 新增 `ignore = "0.4"`；**实施第一步：`cargo add` 后把 lockfile 锁定的精确版本记回本文档，并针对该版本复核 `WalkBuilder` 的默认值与方法集** |
| `crates/veda-cli/src/main.rs` `Commands::Cp` | 新增 `#[arg(long)] no_ignore: bool` |
| `crates/veda-cli/src/main.rs` `Commands::Cp` 分支（:1426） | 透传 `no_ignore` 给 `cp_dir_recursive` |
| `crates/veda-cli/src/main.rs` `cp_dir_recursive`（:1886） | 签名加 `no_ignore: bool`；去掉 `ignored` 计数，改 D7 文案 |
| `crates/veda-cli/src/main.rs` `collect_files`（:1947） | **重写**为 `ignore::WalkBuilder` + `filter_entry`，保留 symlink 跳过 |
| `crates/veda-cli/src/main.rs` 常量（:1941-1945） | `IGNORED_DIRS` / `IGNORED_FILES` 保留不动 |

`collect_files` 重写后的骨架：

```rust
fn collect_files(
    root: &std::path::Path,
    out: &mut Vec<std::path::PathBuf>,
    no_ignore: bool,
) -> anyhow::Result<()> {
    let mut b = ignore::WalkBuilder::new(root);
    b.hidden(false)          // D3: dotfiles are real content in a knowledge base
     .require_git(false)     // D2: honour .gitignore outside git repos too
     .parents(false)         // D4: never read ignore files above the source root
     .ignore(false)          // D4: .ignore is a ripgrep convention, not ours
     .git_global(false)      // D4: uploads must not depend on this machine's git config
     .git_exclude(false)     // D4
     .git_ignore(!no_ignore) // D6
     .follow_links(false)    // D5
     .sort_by_file_path(|a, b| a.cmp(b)); // D8
    if !no_ignore {
        b.add_custom_ignore_filename(".vedaignore");
    }
    // D1: prune built-in skip dirs BEFORE descending. Doing this in the
    // loop below instead would let the walker descend into .git and yield
    // .git/config — whose file name is "config", not in IGNORED_DIRS — so
    // the whole directory would be uploaded. depth 0 is the user-specified
    // source root and must never be filtered out.
    b.filter_entry(|e| {
        if e.depth() == 0 {
            return true;
        }
        let name = e.file_name().to_string_lossy();
        let is_dir = e.file_type().is_some_and(|t| t.is_dir());
        if is_dir {
            !IGNORED_DIRS.contains(&name.as_ref())
        } else {
            !IGNORED_FILES.contains(&name.as_ref())
        }
    });
    for entry in b.build() {
        // symlink skip (D5) + push files
    }
    Ok(())
}
```

`filter_entry` 是 D1 正确性的**唯一**保证点——兜底列表的判断绝不能只放在循环里。

`--no-ignore` 下 `filter_entry` 仍然生效（D6：`.git/` 无论如何不传）。

## 2.5 DoD（需求一）

### 单元测试（`crates/veda-cli/src/main.rs` 内联 `mod` + `tempfile`，`cargo test -p veda-cli`）

用 `tempfile::TempDir` 造目录树，断言 `collect_files` 的结果集合：

| # | 场景 | 期望 | 守的是 |
| --- | --- | --- | --- |
| 1 | `.gitignore` 含 `target/`，树里有 `target/debug/x` | `target/debug/x` 不在结果里 | 核心需求 |
| 2 | `.vedaignore` 含 `*.log`，树里有 `a.log` | `a.log` 不在结果里 | 核心需求 |
| 3 | 树里有 `.github/workflows/ci.yml`，无 ignore 文件 | **在**结果里 | D3 回归防线 |
| 4 | 树里有 `.git/config` + `.git/objects/ab/cd`，**无** `.gitignore` | 两者都不在结果里 | **D1 的真 bug 防线**——只测 `.git/config` 不够，要测深层文件，才能证明是剪枝而非名字匹配 |
| 5 | 目录**不是** git 仓库但有 `.gitignore` 含 `skip.txt` | `skip.txt` 不在结果里 | D2 回归防线 |
| 6 | `no_ignore=true` + `.gitignore` 含 `target/` | `target/debug/x` **在**结果里；`.git/config` **仍不在** | D6 |
| 7 | 树里有指向外部目录的 symlink（含指向目录的） | 不在结果里，不递归进去，stderr 有 `skip symlink` | D5 |
| 8 | 子目录里另有 `.gitignore` | 子目录规则生效（树内层级） | D4 |
| 9 | **源根的父目录**放一个 `.gitignore` 含 `keep.txt`，源根内有 `keep.txt` | `keep.txt` **在**结果里（祖先规则不生效） | **D4 边界**——codex 指出的越界读取 |
| 10 | 源根内有 `.ignore` 含 `x.txt` | `x.txt` **在**结果里（不读 `.ignore`） | D4 |

4、9 是 v2 新增：#4 守 codex 抓出的真 bug，#9 守 `parents(false)` 的边界。3、5 守两个静默回归。这四条都不能省。

`root` 本身是 symlink 的场景：`cp` 分支的 `src_path.is_dir()` 跟随 symlink，`filter_entry` 的 `depth() == 0` 放行，行为与现状一致——**在 #7 里附带断言**，不单列。

### 手工验收（真实 server）

```bash
cargo build -p veda-cli
./target/debug/veda cp . /selftest-gitignore
./target/debug/veda ls /selftest-gitignore
```

**通过判据**：
1. 上传过程 stderr 打出 `(skipped N entries via .gitignore/... )`，N 远大于 0
2. `veda ls /selftest-gitignore` 输出里**没有** `target`
3. `veda ls /selftest-gitignore` 输出里**有** `.github`（D3）
4. 清理：`veda rm /selftest-gitignore`

---

# 需求二：workspace 级 map

## 3.1 定位

**map = 确定性组装的根级视图**，补上 §1.1 说的"根目录没有 L1"这个洞，且不用去造根 summary。

一次调用给出：这个知识库有哪些顶层区域、每个区域大概是什么、有多大。目标读者是 **MCP 接入的 coding agent**（接上陌生 workspace 时的第一次调用）。

## 3.2 API 契约

```
GET /v1/layout
Authorization: Bearer wk_...
```

响应（`ApiResponse<WorkspaceMap>` 包一层，与其他 `/v1/*` 一致）。**注意实际 envelope 是 `success`/`data`/`error_code`/`error`**（`veda-types/src/errors.rs:87`），不是 `code`：

```json
{
  "success": true,
  "data": {
    "stats": { "total_files": 1234, "total_directories": 56, "total_bytes": 7890123 },
    "summary_state": "partial",
    "truncated": false,
    "entries": [
      { "path": "/docs", "is_dir": true, "abstract": "veda 的设计与部署文档...", "file_count": 42 },
      { "path": "/wiki", "is_dir": true, "file_count": 310 },
      { "path": "/README.md", "is_dir": false, "abstract": "项目总览...", "size_bytes": 4096 }
    ]
  }
}
```

字段语义：

| 字段 | 说明 |
| --- | --- |
| `stats` | 复用现有 `MetadataStore::storage_stats`（`admin.rs:142` 已在用），零新代码 |
| `summary_state` | `ready` / `partial` / `disabled`，见 D10 |
| `truncated` | 顶层条目超过 `MAP_ENTRY_CAP` 被截断，见 D11 |
| `entries[].abstract` | L0，缺失时**省略该 key**（`skip_serializing_if = "Option::is_none"`），不是 `null` |
| `entries[].file_count` | 仅目录有；该顶层目录下的文件总数（递归），见 D12 |
| `entries[].size_bytes` | 仅文件有 |

### D9. 深度固定一层，**不做** `?depth=`

只返回顶层（`parent_path = "/"`）条目。

**不做会怎样**：agent 想深入 `/docs` 时要再调一次 `overview /docs` 或 `list_dir /docs`——一次额外调用，可接受。

反过来支持 `?depth=2` 的代价：20 个顶层目录 × 每个 30 个子项 = 600 条 × ~100 token abstract ≈ 60k token，一次调用就把 agent 的 context 打爆。这个参数是"以后可能有人要"，现在没有需求，不做。

### D10. 摘要 pending / disabled：**永远 200**，用 body 字段表达

map 是 N 个摘要的聚合，不可能沿用 abstract/overview 的三态 HTTP（200/202/501）。

```rust
#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MapSummaryState { Ready, Partial, Disabled }
```

语义（**v1 的定义自相矛盾，此处重定**）：

| `summary_state` | 含义 | 触发条件 | entries 里的 abstract |
| --- | --- | --- | --- |
| `ready` | 返回的每个 entry 都有 L0 | `summary_enabled` 且覆盖率 100% | 全部有 |
| `partial` | **返回条目的覆盖率不完整** | `summary_enabled` 且覆盖率 < 100% | 有的有有的没有 |
| `disabled` | **不会再生成新摘要** | `summary_enabled == false` | **已缓存的照常返回** |

两处关键修正（codex 评审）：

1. **`disabled` 不抹掉已有 abstract。** v1 写的"disabled → 全部省略"是错的：`[llm]` 被摘掉后，历史生成的 summary 行仍在库里，而 `/v1/abstract` 在有 summary 时**根本不看 `summary_enabled`** 直接返回（`search.rs:75`）。map 把它们藏起来会和同一台 server 上的 abstract 端点自相矛盾。`disabled` 只表达"别等了，不会有新的"。
2. **`partial` 不等于"稍后重试就会好"。** 空目录的摘要会被 worker 主动删除（`worker.rs:637`，避免留下 pre-empty 的陈旧聚合），这类条目**永远**不会有 L0。所以 `partial` 定义成"覆盖率不完整"这个事实陈述，不承诺重试有用。

**LLM 未配时 map 仍返回 200 且仍有价值**——退化成"顶层目录树 + 文件计数 + 总量统计"，仍比连调十次 `list_dir` 强。

**不返回 501 的理由**：新建 workspace 刚 cp 完、summary 还在排队时就 501 是最糟的首次体验；`summary_state` 已经把状态讲清楚了。

### D11. 规模上限：`MAP_ENTRY_CAP = 200`，**上限必须前推到 store 查询**

200 条 × ~100 token abstract ≈ 20k token，已经是 agent 单次调用能吃的上限。

超过则截断并置 `truncated: true`。**排序规则：目录在前、文件在后，各自按 path 字典序。** 截断时优先保住目录——目录是"区域"，信息密度远高于散落在根下的单个文件。

**v1 的设计有实质缺陷（codex 抓出）**：v1 是"5 次读取全做完，最后排序截断 200"。但 `list_dentries` 是**无 LIMIT 的 `fetch_all`**（`mysql.rs:955`）——根下散着 5 万个文件的 workspace 会先把 5 万行 dentry 全读进内存，再拿 5 万个 file_id 去拼 `IN (...)`，再读全部根摘要，然后才截断到 200。内存、SQL 参数数量、延迟三重风险。

这个坑本仓库踩过：`list_dir_recursive` 的注释写着「Old code did load-all-then-check, which OOMed on huge workspaces (review C2)」。

**正确做法**：cap 在第一步就生效，后续每一步只处理这 ≤200 条。

```
1. list_children_capped(ws, "/", CAP + 1)     -- SQL 层 ORDER BY is_dir DESC, path LIMIT 201
2. truncated = (返回行数 > CAP)，截到 CAP
3. 只对这 ≤200 条取 file metadata / summary
```

多取 1 行（201）用来判断是否截断，避免额外一次 COUNT。

`stats`（`storage_stats`）与 `file_count`（D12）仍是全 workspace 聚合，天然 `O(N)`——见 §5 风险表，不在此处解决。

参考 MCP `list_dir` flat 模式的 `truncated` 语义（`mcp.rs:724` 附近），保持一致。

### D12. `file_count`：做，一条 GROUP BY，但**成本要说实话**

**不做会怎样**：agent 看到 `/docs` 和 `/wiki` 两个目录，不知道一个是 3 个文件、一个是 3000 个，无法决定该 `list_dir` 还是 `search`。这直接影响它下一步的 token 花销。

**成本更正（v1 说错了）**：v1 称"走现有 `idx_workspace` 索引"——`veda_dentries` **根本没有** `idx_workspace`（那是 `veda_files` 上的）。该表的索引是：

```
UNIQUE INDEX idx_ws_path (workspace_id, path_hash)
INDEX idx_parent (workspace_id, parent_path(255))
INDEX idx_ws_path_prefix (workspace_id, path(255))
```

`GROUP BY SUBSTRING_INDEX(...)` 是表达式分组，**没有可用的分组索引**。最好情况是靠复合索引左前缀把行限定到本 workspace，然后**全量扫描该 workspace 的所有 dentry** 来过滤 `is_dir` 并分组。即 `O(workspace dentry 数)`。

**仍然做的理由**：现有 `storage_stats`（`mysql.rs:1316`，map 也要调）本来就是同量级的全 workspace 扫描 + join files。D12 没有引入新的复杂度量级，只是多一次同量级扫描。

**但要验证**：见 §5 风险表——上线前在真实生产量级上 `EXPLAIN ANALYZE`，若延迟不可接受，退路是砍掉 `file_count`（map 的核心价值在 abstract，不在计数）。

### D13. 鉴权与 kind：`AuthWorkspace`，无需额外处理

`AuthWorkspace` extractor 已内建 fs-only 校验，db kind 的 `wk_` 自动返 400。与 `/v1/search`、`/v1/answer`、`/mcp` 完全一致，**不需要在 handler 里写任何 kind 判断**。

错误码是**大写** `WORKSPACE_KIND_MISMATCH`（`auth.rs:299`，v1 写成小写是错的）。注意鉴权顺序：缺失 / 无效 Bearer 在 kind 检查**之前**就返回 401（`auth.rs:202`），只有拿到有效 `wk_` 且 `kind != Fs` 才会走到 400。

### D14. 平台网关面 / tunnel：先不做

同意原始判断，理由补充：

- 平台面 `project_data.rs` 服务的是前端 UI，UI 已有目录浏览器，map 对它价值低
- tunnel 的"你知道些什么"是真需求，但 **tunnel 是标准 `wk_` 消费者，它可以直接调 `/v1/layout`**——server 侧零额外工作。要不要接是 tunnel 的独立决定，不属于本方案
- 不做的代价：需要时在 `project_data.rs` 加一行路由，几分钟的事

同理**不做 CLI 子命令**（`veda layout`）。原始需求没提，且 CLI 用户有 `veda ls` + `veda overview`。等 MCP 侧验证价值后再说。

## 3.3 MCP 工具

### 工具 description（D15）

```
How this knowledge base is organised: its top-level areas, each with a one-line
summary and file count per area. Call this FIRST when you don't yet know what
the workspace contains — one call replaces a round of list_dir probing and tells
you which subtree to search or read. Cheap: ~100 tokens per entry.
```

`inputSchema` 为空（`{"type": "object", "properties": {}}`），无参数。`annotations: { readOnlyHint: true }`。

### `initialize` 的 instructions 也要改

`mcp.rs:349` 现在是：

```
Start with `search` (detail_level='abstract' scans relevance at ~100 tokens/hit), ...
```

改为：

```
Call `layout` first to see how an unfamiliar workspace is organised, then `search`
(detail_level='abstract' scans relevance at ~100 tokens/hit),
then `read_file` the promising paths. ...
```

这是 map 能被真正用起来的关键——工具存在但 instructions 不提，agent 大概率不会主动调。

### 返回格式

与其他 MCP 工具一致：`to_json_text(&map_json)`，即把 `WorkspaceMap` 序列化成 JSON 文本放进 `content[0].text`。**不做**给 LLM 看的 markdown 渲染——JSON 更省 token 且结构明确。

## 3.4 改动清单

### store 层（`veda-core/src/store.rs` + `veda-store/src/mysql.rs`）

> **v1 的 D16（改 `list_child_summaries` 签名带 path）整条删除。**
> 把 cap 前推到 store 层（D11）之后，流程变成"先拿 ≤200 条 dentry，再按这些 id 批量取 summary"——
> 根本不需要一个"按 parent_path 捞全部子摘要"的方法。删掉 D16 的收益：
> `list_child_summaries` 不动、`worker.rs:635` 不动、3 处 mock 不动，改动面净减少，
> 且每一步都有界。这是被 D11 逼出来的更好设计。

**D16（新）. `list_children_capped` —— 有界的直接子节点列举**

```rust
/// Direct children of `parent_path`, directories first then files, each
/// group ordered by path, capped at `limit`. The map endpoint needs a
/// bounded read: `list_dentries` is an unbounded fetch_all and a workspace
/// with 50k root-level files would load all of them just to show 200.
async fn list_children_capped(
    &self,
    workspace_id: &str,
    parent_path: &str,
    limit: usize,
) -> Result<Vec<Dentry>>;
```

MySQL 实现走现有 `idx_parent (workspace_id, parent_path(255))`：

```sql
SELECT id, workspace_id, parent_path, name, path, file_id, is_dir, created_at, updated_at
FROM veda_dentries
WHERE workspace_id = ? AND parent_path = ?
ORDER BY is_dir DESC, path
LIMIT ?
```

排序下推到 SQL，`ORDER BY is_dir DESC` 让目录排在前（D11 的截断策略在 SQL 层就生效，而不是读回来再排）。

**D17. `get_summaries_by_dentry_ids` —— 目录摘要的批量版**

`get_summaries_by_file_ids` 已存在且 **MySQL 已覆盖为真批量**（`mysql.rs:1376`，不是 trait 上那个 N+1 的默认实现），文件侧直接复用。目录侧只有单条的 `get_summary_by_dentry`，需要一个对称的批量版：

```rust
/// Batch sibling of `get_summaries_by_file_ids`, keyed by dentry_id.
/// Directory summaries are keyed by dentry, not file.
async fn get_summaries_by_dentry_ids(
    &self,
    dentry_ids: &[String],
) -> Result<std::collections::HashMap<String, FileSummary>>;
```

MySQL 实现照抄 `get_summaries_by_file_ids`，把 `file_id IN (...)` 换成 `dentry_id IN (...)`。**不给 trait 默认实现**——默认实现会退化成 N+1 且没人会发现。

**D18. `count_files_by_top_level`**

```rust
/// File counts grouped by top-level path segment ("/docs/a/b.md" -> "docs").
/// One query — the map endpoint needs "how big is each area" without
/// walking the tree. Cost is O(dentries in workspace): the GROUP BY is on
/// an expression, so no index can serve the grouping (see plan D12).
async fn count_files_by_top_level(
    &self,
    workspace_id: &str,
) -> Result<std::collections::HashMap<String, i64>>;
```

MySQL 实现：

```sql
SELECT SUBSTRING_INDEX(SUBSTRING(path, 2), '/', 1) AS top_seg, COUNT(*) AS n
FROM veda_dentries
WHERE workspace_id = ? AND is_dir = 0
GROUP BY top_seg
```

用了 MySQL 特有函数——`veda-store` 本来就是 MySQL-only 实现，可接受。

**切分的正确性**（codex 复核确认）：路径正规化（`veda-core/src/path.rs`）把 `/` 固定为结构分隔符，常规 API 写不出"含 `/` 的单段名"，所以 `SUBSTRING_INDEX` 的切分对规范路径是正确的。根下的文件 `/README.md` 会分到 key `README.md`——**这是期望行为**，但组装时**只给 `is_dir == true` 的 entry 读这个 count**，不能按"map 里有没有这个 key"来判断。

**mock 影响**：D16/D17/D18 三个新方法各需在 3 处 mock 加实现（`veda-core/tests/mock_store.rs`、`veda-sql/tests/sql_test.rs` ×2）。其中 `veda-core` 的 mock **必须可配置**（不能像现有 `list_child_summaries` 那样恒返空），否则需求二的单测跑不出 `ready`/`partial`/`file_count` —— 见 DoD。`veda-sql` 的两处与 map 无关，返回空即可。

### core 服务层（`veda-core/src/service/search.rs`）

```rust
pub async fn workspace_map(&self, workspace_id: &str) -> Result<WorkspaceMap>
```

放 `SearchService` 而不是 `FsService`：map 的核心是摘要聚合，`SearchService` 已经持有 `self.meta` 且已是 `get_summary` 的家。**不引入 service 间依赖**（SearchService 不该持有 FsService）。

组装流程（全部走 `self.meta`，**每一步都有界**）：

1. `list_children_capped(ws, "/", CAP + 1)` → ≤201 条顶层 dentry，SQL 已按「目录优先 + path」排好
2. `truncated = rows.len() > CAP`，截到 CAP
3. 从这 ≤200 条里分出 `file_ids` / `dentry_ids`（目录）
4. `get_files_batch(&file_ids)` → size_bytes
5. `get_summaries_by_file_ids(&file_ids)` + `get_summaries_by_dentry_ids(&dentry_ids)` → L0
6. `count_files_by_top_level(ws)` → 目录的 file_count（**仅 `is_dir` 的 entry 读**）
7. `storage_stats(ws)` → 总量
8. 计算 `summary_state`（覆盖率基于**返回的** entries，不是全 workspace）

6 次 store 往返。1/4/5 有界（≤200），6/7 是全 workspace 聚合（D12 已说明其 `O(N)` 成本）。无 N+1。

**一致性**（codex 提出）：这 6 次读取**不是一致性快照**——并发写入时 entries / summary / counts / stats 可能来自不同时间点。按项目既定的最终一致性策略，v0 接受，但要在对外文档写明"map 是尽力而为的快照，不保证各字段同一时刻一致"。**任一步失败即整个请求 500**（现有 `AppError` 已把 storage 错误映射成 500 INTERNAL，`error.rs:24`），不返回混合的部分结果。不为此引入跨层事务。

`summary_enabled` 是 server 层状态（`AppState.summary_enabled`），core 拿不到 → `workspace_map` 只区分 `ready` / `partial`，**由 server handler 在 `summary_enabled == false` 时覆写为 `disabled`**。注意按 D10：覆写的只是 `summary_state` 这个字段，**不清空已缓存的 abstract**。

### types（`veda-types/src/api.rs`）

```rust
#[derive(Debug, Serialize)]
pub struct WorkspaceMap {
    pub stats: StorageStats,
    pub summary_state: MapSummaryState,   // ready | partial | disabled
    pub truncated: bool,
    pub entries: Vec<MapEntry>,
}

#[derive(Debug, Serialize)]
pub struct MapEntry {
    pub path: String,
    pub is_dir: bool,
    #[serde(rename = "abstract", skip_serializing_if = "Option::is_none")]
    pub l0_abstract: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_count: Option<i64>,   // dirs only
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<i64>,   // files only
}
```

注意 `abstract` 是 Rust 保留字，字段名用 `l0_abstract` + `#[serde(rename = "abstract")]`。

### server 层

| 文件 | 改动 |
| --- | --- |
| `veda-server/src/routes/search.rs` | `routes()` 加 `.route("/v1/layout", get(get_map))`；新增 `get_map` handler（`AuthWorkspace`，调 `workspace_map`，按 `state.summary_enabled` 覆写 `summary_state`） |
| `veda-server/src/routes/mcp.rs` `tool_metric_label` | 加 `"layout" => "tool:layout"` |
| `veda-server/src/routes/mcp.rs` `tool_specs` | 加 map 的 spec（D15 文案），**放在数组第一位**——tools/list 顺序影响 LLM 的默认倾向 |
| `veda-server/src/routes/mcp.rs` `run_tool` | 加 `"map" => tool_map(state, auth).await` |
| `veda-server/src/routes/mcp.rs` `initialize_result` | instructions 改成 map-first（D15） |
| `veda-server/src/routes/mcp.rs` | 新增 `tool_map`（无参数，直接 `to_json_text`） |

放 `search.rs` 而非新建 `map.rs`：该文件已经是 abstract/overview 这类"summary 读端点"的家，map 是同族，单开一个只有一个 handler 的文件不值得。

### 文档

| 文件 | 改动 |
| --- | --- |
| `ARCHITECTURE.md` | MCP 章节 6 个工具改 7 个；已实现能力里补 `/v1/layout` |
| `web/public/docs/{zh,en}/reference.md` | **对外权威契约写这里**（含 D10 的 `summary_state` 语义、`truncated`、非快照声明） |
| `docs/api/db-workspace-api.md` | **只在 §9「不属于本 API 的端点」清单里加 `GET /v1/layout`**——该文件自述"只覆盖 db workspace"，把 fs 端点契约写进去会把两条数据面混在一起（v1 写错了） |
| `CHANGELOG.md` | `[Unreleased]` 加两条（cp ignore + map） |

## 3.5 DoD（需求二）

> **前置改造（v1 的 DoD 在现有测试设施下跑不出来，codex 抓出）**：
> - `veda-core/tests/mock_store.rs` 的 summary 相关方法目前**恒返空**（`list_child_summaries` 直接 `Ok(vec![])`）。
>   新增的 D16/D17/D18 三个方法在这个 mock 里**必须由测试注入数据**（例如 `MockStore` 加
>   `summaries: Mutex<HashMap<..>>` / `top_level_counts: Mutex<HashMap<..>>` 字段 + setter），
>   否则下面 #3/#4/#5 永远只能测出"全空"。
> - `crates/veda-server/tests/mcp_http_test.rs` 写死 `summary_enabled: false`（:172），
>   **产不出 `ready`**。集成测试必须**新建 `crates/veda-server/tests/map_test.rs`**，
>   自己搭 `summary_enabled: true` 的 AppState，不要往 mcp_http_test 里塞。

### 单元测试（`cargo test`，走 mock store）

`crates/veda-core/tests/` 里针对 `workspace_map` 组装逻辑：

| # | 场景 | 期望 |
| --- | --- | --- |
| 1 | 混合顶层：2 目录 2 文件 | entries 顺序 = 目录（字典序）→ 文件（字典序） |
| 2 | 250 个顶层目录（mock 按 limit 截断） | entries 长度 200，`truncated == true`，且**断言 mock 收到的 `limit == 201`**（守 D11 的"cap 前推"，不能只测结果长度——那样退回 v1 的 load-all 也能过） |
| 3 | 注入：全部 entry 都有 ready summary | `summary_state == ready` |
| 4 | 注入：一半有 summary | `summary_state == partial`，无摘要的 entry 序列化后**没有** `abstract` key |
| 5 | 注入 top-level counts；目录 + 文件混合 | 目录 entry 有 `file_count` 无 `size_bytes`；文件 entry 反之。**根下文件的 path 段恰好也是 counts 的 key 时，该文件 entry 仍不带 `file_count`**（守 D18 的"只给 is_dir 读 count"） |
| 6 | 空 workspace | entries 空，不报错，`stats` 全 0，`truncated == false` |
| 7 | `summary_state` 覆盖率基于**返回的** entries | 250 个目录只有前 200 有 summary → `ready`（不因被截断的那 50 个而变 `partial`） |

server handler 单测：

| # | 场景 | 期望 |
| --- | --- | --- |
| 8 | `summary_enabled == false` 且 store 里**有**缓存的 L0 | `summary_state == disabled`，但 entries 的 `abstract` **照常返回**（守 D10 修正点 1） |

MCP 侧单测（`mcp.rs` 内联，与现有 9 个协议单测同处）：

| # | 场景 | 期望 |
| --- | --- | --- |
| 9 | `tools/list` | 含 `map`，且 `map` 在数组**第一位**，`readOnlyHint == true` |
| 10 | `tool_metric_label("layout")` | 返回 `"tool:layout"`（防 metrics 基数逃逸） |
| 11 | `initialize` 的 `instructions` | 含 `map`（守 D15——工具存在但 instructions 不提，agent 不会调） |

### 集成测试（真实 MySQL / Milvus / embedding / LLM）

新建 `crates/veda-server/tests/map_test.rs`，跑法（项目约定，见 CLAUDE.md + `config/test.toml`）：

```bash
NO_PROXY='*' cargo test -p veda-server --test map_test -- --ignored --test-threads=1
```

| # | 场景 | 期望 |
| --- | --- | --- |
| 12 | 建 fs workspace（`summary_enabled: true`）→ 写 `/docs/a.md`、`/docs/b.md`、`/wiki/c.md`、`/README.md` → 等 worker 出 summary → `GET /v1/layout` | 200；`success == true`；entries 含 `/docs`（`file_count == 2`）、`/wiki`（`file_count == 1`）、`/README.md`（有 `size_bytes`、**无** `file_count`）；顺序目录先；`summary_state == ready`；`stats.total_files == 4` |
| 13 | summary 还没生成完时立刻 `GET /v1/layout` | 200（**不是** 202/501），`summary_state == partial` |
| 14 | db kind 的 `wk_` 调 `GET /v1/layout` | 400 且 `error_code == "WORKSPACE_KIND_MISMATCH"` |
| 15 | 无 Authorization header | 401（**在** kind 检查之前，见 D13） |
| 16 | MCP `tools/call` name=`map` | `isError == false`，`content[0].text` 解析出的 JSON 与 REST 响应的 **`data` 字段**同构 |
| 17 | 根下写 250 个文件 | entries 长度 200，`truncated == true`，响应正常返回不 OOM |

**通过判据**：上面命令全绿。其中不可省的三条：

- **#12 的 `file_count` 与实际写入数一致** —— D18 那条 `SUBSTRING_INDEX` SQL 唯一的正确性验证点，必须跑真 MySQL
- **#2 的 `limit == 201` 断言** —— D11「cap 前推」唯一的验证点，只看结果长度测不出来
- **#8 的 disabled 仍返 abstract** —— D10 修正点唯一的验证点

---

## 4. 实施顺序

1. **需求一**：`cargo add ignore` → **记录 lockfile 精确版本并复核默认值** → `collect_files` 重写（`filter_entry` 是关键）→ 单测 10 条 → 手工验收
2. **提交点 A**（需求一独立可发，与需求二无耦合）
3. **需求二 store 层**：D16/D17/D18 三个新方法 + MySQL 实现 + 3 处 mock（`veda-core` 的那处要**可注入数据**）
4. **需求二 core**：`workspace_map` + 单测 1-7
5. **需求二 server**：REST 路由 + MCP 工具 + instructions + 单测 8-11
6. **需求二集成测试**：新建 `map_test.rs`，12-17
7. **文档**：ARCHITECTURE / reference.md（中英，权威契约）/ db-workspace-api.md §9 排除清单 / CHANGELOG
8. **提交点 B**

需求一不涉及 server，**CLI 单独发版即可**；需求二要 server 升级。两者不必同一个版本上线。

---

## 5. 风险与不确定点

| 风险 | 评估 |
| --- | --- |
| **兜底列表未用 `filter_entry` 剪枝** | **最高风险，且 v1 方案原本就写错了**。放在循环里判断的话 walker 会下降进 `.git/`，而 `.git/config` 的条目名是 `config` 不在列表里 → 整个 `.git` 被上传。单测 #4 必须测深层文件（`.git/objects/ab/cd`），只测 `.git/config` 证明不了是剪枝 |
| `ignore` crate 的 `hidden` / `require_git` 默认值被实施时用回默认 | 次高。D2/D3 是静默行为改变，单测 #3 #5 是专门的回归防线。已独立核验 0.4.31 源码 `dir.rs::IgnoreBuilder::new()`：`hidden: true`、`require_git: true`。**实施第一步仍要按 lockfile 锁定的精确版本复核一次**（codex 正确指出方案定稿时该版本尚未进 lockfile） |
| D18 的 `GROUP BY SUBSTRING_INDEX(...)` 是 `O(workspace dentry 数)` 全扫 | 已确认无可用分组索引（D12）。同量级的扫描 `storage_stats` 本来就在做，未引入新量级。**但上线前要在真实生产量级上 `EXPLAIN ANALYZE`**；若延迟不可接受，退路是砍掉 `file_count`（map 的核心价值在 abstract 不在计数） |
| `MAP_ENTRY_CAP = 200` 是拍的 | 没有生产数据支撑。**实施后在 .161 上对真实 workspace 跑一次看 `truncated` 是否触发**，若普遍触发再调 |
| map 的 6 次读取非一致性快照 | v0 接受（项目既定最终一致性策略），但必须在对外文档写明，且任一步失败整体 500 不返回部分结果 |
| map 对 tunnel 的"你知道些什么"到底够不够 | 本方案不含 tunnel 改动。map 返回 JSON，tunnel 要用得自己渲染成中文话术——tunnel 侧的独立工作，价值待验证 |

**已消除的 v1 风险**：
- ~~`list_child_summaries` 签名改动波及 3 个 mock + worker~~ —— D16 已删，这条不存在了
- ~~map 只截响应不截读取导致 OOM~~ —— D11 已把 cap 前推到 SQL `LIMIT`

---

## 6. 明确不做的（防镀金）

- `?depth=` 参数（D9）
- markdown 渲染的 map 输出（JSON 够用且更省 token）
- 根目录 L1 summary 的 worker/store 支持（§1.1——map 就是为了不做这个）
- `veda layout` CLI 子命令（D14）
- 平台网关面 / tunnel 的 map 接入（D14）
- `--no-ignore-vcs` / `--unrestricted` 分级逃生舱（D6）
- 跳过原因的分类计数（D7）
- **v2 新增**：跨层事务 / 一致性快照读（§3.4——v0 明确接受非快照）
- **v2 新增**：`.gitignore` 祖先边界"到 repo root 为止"的实现（D4——直接 `parents(false)` 更简单）

## 7. codex 方案评审中**未采纳**的意见

按 CLAUDE.md 评审约定，记下没改的和原因：

| 意见 | 判断 | 理由 |
| --- | --- | --- |
| "ignore crate 版本 UNVERIFIED，不应把默认值写成已验证事实" | **部分采纳** | 已独立核验 0.4.31 源码 `dir.rs::IgnoreBuilder::new()` 的 `IgnoreOptions` 字面量（`hidden: true` / `require_git: true`），`cargo add --dry-run` 也解析到 0.4.31，不是无依据推断。**采纳的部分**：实施第一步按 lockfile 锁定版本再复核一次，已写进 §2.4 和 §5 |
| "DirEntry 文档没有 `ignore()` 方法" | **不采纳** | 查错了类型。`ignore()` 是 `WalkBuilder` 的方法（控制是否读 `.ignore` 文件），方案从未提议 `DirEntry::ignore()` |
| D4 "要么接受所有祖先规则并写进帮助和测试，要么实现边界到 repo root" | **不采纳，换更简单解法** | 两个选项都比 `parents(false)` 复杂。只读源目录树内的 ignore 文件，行为一句话讲得清、测试简单，且顺带消灭了跨机器不可复现的问题 |
