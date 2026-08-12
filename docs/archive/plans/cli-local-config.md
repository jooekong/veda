# CLI 目录级配置（`.veda.toml`）

> 2026-08-12 提出并当日实现（单测 + 冒烟全过，见文末实现注记）。
> 目标：目录级配置正规化，取代目前用临时 `XDG_CONFIG_HOME` 做配置隔离的 hack。

## 目标 / 完成判据

- 在含 `.veda.toml` 的目录（或其任意子目录）里跑 `veda`，读写完全以该文件为准；
  不存在则回落全局 `~/.config/veda/config.toml`，行为与现在完全一致。
- `veda status` / `veda config show` 能看出当前生效的是哪个配置文件。

## 已拍板（2026-08-12，Joe）

| 分叉 | 决定 | 理由 |
| --- | --- | --- |
| 语义 | **整文件替换**，不做字段 merge / alias 引用 | 最简；目录配置永远碰不到全局 key——字段 merge 存在「恶意仓库放个只改 server_url 的 .veda.toml，全局 wk_ 被叠加发给对方」的凭证外泄面 |
| 查找 | **从 CWD 向上递归**到文件系统根，首个命中生效（类 git） | 只查 CWD 会在项目子目录里静默回落全局，写错 workspace |
| 文件名 | `.veda.toml`（dotfile 单文件，不搞 `.veda/` 目录） | 一个文件够用 |
| agent 缓解一 | 加 `$VEDA_CONFIG=<file>` 显式钉定配置文件（CWD 无关），优先级在 walk-up 之上 | agent 的 CWD 会漂到项目树外（scratchpad/tmp）导致静默回落全局；一行 env 全解，也是 `XDG_CONFIG_HOME` hack 的精确替代 |
| agent 缓解二 | 文档推荐「可提交无密钥」模式：`.veda.toml` 只写 server_url 并提交进 repo，key 走 `$VEDA_KEY` | gitignore（含 key 时必须）与 worktree / 新 clone 结构性冲突——ignored 文件不进 worktree，静默回落全局；无密钥文件可提交，且漏配 key 时是响亮报错而非静默打全局 |

## 设计

### 解析

- `config.rs` 新增 `resolve_path(env_pin, from_dir) -> (PathBuf, ConfigSource)`，
  `ConfigSource = Global | Local | EnvPin`：
  - `$VEDA_CONFIG` 非空 → `EnvPin(该文件)`，与目录无关；
  - 否则从 `from_dir` 向上逐级找 `.veda.toml`，命中 → `Local(path)`；
  - 未命中 → `default_path()`（现逻辑不动，含 mkdir）。
  - 签名带 `from_dir` / `env_pin` 参数（`load()` 传 `current_dir()` 和真实 env）
    是刻意的：测试不碰进程 cwd/env，避免 cargo test 并行下全局态互踩。
- `CliConfig` 记住来源：`source_path: PathBuf` + `source: ConfigSource`。
  现有 `assert_eq!(loaded, saved)` 类测试受 derive PartialEq 影响，实现时
  要么手写 PartialEq 忽略 source，要么测试侧补齐 source 再比，取实现时更省的。
- `load_from` / `save_to` 纯函数语义不变（fuse 与现有测试不受影响）。

### 语义

- `.veda.toml` schema 与全局 config.toml **完全一致**（复用 `RawConfig`，含 legacy
  迁移逻辑），即自带 `server_url` + `workspaces` + `active_workspace`（`api_key` 可选）。
- 优先级链：`--server` / `--workspace` flag > `$VEDA_SERVER` / `$VEDA_KEY` >
  **`$VEDA_CONFIG` 钉定文件 > `.veda.toml`（就近优先）** > 全局 config.toml。
  env 凭证仍压过任何文件，CI / 脚本 export 变量的既有语义不破坏。
- `.veda.toml` 存在但解析失败：**报错退出**，绝不静默回落全局（静默回落 = 打错
  workspace，比报错糟糕得多）。
- `.veda.toml` 存在但内容空 / 无 workspace：按「配置为空」同样报错引导 `veda init`，
  此时**不回落**全局——文件在即隔离在。

### 写回

- 统一规则：`save()` 写回加载来源。目录模式下 `workspace add/switch/rm`、
  `config set`、`status` 的 workspace id 回填全部落 `.veda.toml`；全局模式行为不变。
- `veda init` 同样遵守统一规则（目录模式下 init 写目录文件），不设特例；
  `main.rs` 三处 `default_path()` 的路径展示改为 resolved path，用户看得到写了哪。
- `save_to` 已有 0600 权限，目录文件同样适用。

### 可见性

- `veda status` 新增 `Config: <path>` 行，目录模式加 `[local]` 标记
  （对齐现有 `[$VEDA_SERVER]` 的标注风格）。
- `veda config show` 同样带来源路径。

### fuse

- veda-fuse 兜底走 `veda_cli::config::CliConfig::load()`，自动继承目录级行为：
  在含 `.veda.toml` 的目录执行 mount 即用该配置。与 CLI 一致，不另做开关，
  fuse 零代码改动；文档提一句即可。

### 安全注记

- `.veda.toml` 通常含 wk_ key：文档明确要求加 `.gitignore`；init 目录模式生成
  文件时头部写一行注释提醒（低成本，实现时顺手）。
- 整文件替换本身保证：恶意仓库内置 `.veda.toml` 拿不到全局 key，最坏是把该目录
  下的上传内容引到它的 server——`status` 的 Config 行可排查。

## 测试

单元测试（纯逻辑，mock 范畴）：

1. 目录有 `.veda.toml` → 用之；无 → 全局（`XDG_CONFIG_HOME` 指 tempdir 验证）。
2. 向上递归：子目录命中祖先目录的文件；全程无命中回落全局。
3. env 仍压过目录配置（`$VEDA_KEY` vs `.veda.toml` 里的 key）。
4. save 写回来源：目录模式 `workspace switch` 后全局文件内容不动。
5. 目录配置解析失败报错，不静默回落。
6. `status` 渲染 Config 行 + `[local]` 标记。
7. 现有 config / status / fuse 测试全绿（`load_from` 语义未变）。

## 文档 / 发布

- `CHANGELOG.md` `[Unreleased]` 记条目。
- web zh CLI 文档补「目录级配置」小节（对外权威）；aidoc 欠账列表顺带记一笔。
- 纯 CLI 改动走 CLI 发版，server 零改动、无需部署。

## 改动面

- `crates/veda-cli/src/config.rs` — resolve + source 字段 + save 路由
- `crates/veda-cli/src/main.rs` — 三处路径展示改 resolved path
- `crates/veda-cli/src/status.rs` — Config 行
- 测试 + docs；veda-fuse 零改动

## 实现注记（2026-08-12）

- 按方案落地，含两条 agent 缓解（`$VEDA_CONFIG` + 可提交无密钥模式文档化）。
  与方案的偏差：
  - `--import-key` 的**备份目标**从 `default_path()` 改为 resolved source——
    方案只提了三处「路径展示」，实现时发现这处是行为而非展示：备份必须对准
    `cfg.save()` 将要覆盖的那个文件，否则目录模式下备份了全局、覆盖了局部。
  - `.gitignore` 提醒做成 `save_to` 按文件名（`.veda.toml`）判断写头部注释，
    每次保存重新生成；`$VEDA_CONFIG` 钉定的自定义文件名不加（显式钉定者自知）。
  - status 对「文件存在但未配置」的局部/钉定文件输出专门文案（named shadowing），
    不复用全局的 "No configuration"。
- 测试：veda-cli 单测 172+88、veda-fuse 26 全绿；冒烟验证 walk-up / `$VEDA_CONFIG`
  钉定 / 写回同源（全局文件不动）/ 空局部遮蔽文案 / 头部注释，全过。
- 文档：`CHANGELOG.md` [Unreleased]、`web/public/docs/zh/cli.md` 新增小节；
  aidoc 同步待发版时一并处理（那边已有 index-status 欠账）。

## Codex review 处置（2026-08-12，Joe 拍板）

- **P2 相对 `$VEDA_CONFIG` 按 CWD 漂移 — 已修**：`resolve_path` 拒绝非绝对路径
  （报错引导），文档标注「必须绝对路径」。
- **P1 `$VEDA_KEY` + 恶意目录 `.veda.toml` 改 server_url = 凭证外泄面 — 拍板不加闸**，
  接受该风险保留「可提交无密钥」模式；文档加了一句提示（设着 `$VEDA_KEY` 别在
  不可信 checkout 跑；export `$VEDA_SERVER` 可彻底免疫，因 env server 压过文件）。
  若后续要重启，方案是「窄闸」：Local 来源 server_url && `$VEDA_KEY` 已设 &&
  `$VEDA_SERVER` 未设 → 硬报错。
