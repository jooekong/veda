# Changelog

All notable changes to Veda are documented here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

This project is in **alpha**: minor versions can break compatibility, on-disk
shapes, and CLI command spelling without prior notice. Pin `VEDA_VERSION` if
that matters.

## [Unreleased]

## [0.1.8] — 2026-05-19

### Changed
- `install.sh`: default install location now follows the
  curl-pipe-sh convention used by gh-cli / k3s / fly.io —
  `/usr/local/bin` for root (already in everyone's PATH),
  `$HOME/.local/bin` for non-root (XDG idiom, banner walks
  through the PATH tweak when needed). `VEDA_INSTALL_DIR`
  still overrides either default.
- `install.sh`: new `--source github|gitlab` flag (PR2's
  env-only approach bit on `VEDA_SOURCE=… curl … | sh`, where
  the env doesn't cross the pipe to `sh`). `--from-github` /
  `--from-gitlab` kept as aliases. CLI flag takes precedence
  over the `VEDA_SOURCE` env var.
- `install.sh`: PATH-missing warning is now an end-of-output
  ASCII-box banner with both immediate (`export PATH=…`) and
  persistent (`echo … >> ~/.bashrc`) remediation. Still
  doesn't auto-edit shell rc files.
- **Reverted the PR3b skill split.** `skill-fuse.md` merged
  back into `skill.md` (now 331 lines, was 369 across two
  files). The split was over-engineering: Claude Code's skill
  loader only auto-registers `~/.claude/skills/<dir>/SKILL.md`
  — additional .md files in the same directory don't trigger
  on their own keywords. PR3b's net token saving was 3 lines
  while introducing skill-discovery edge cases, conditional
  `install.sh` fetch, two CI artifacts, and cross-link
  maintenance.

### Added
- `install.sh` `preflight_fuse` recognizes more RHEL-family
  forks common on internal Chinese clouds: `hce` (Huawei
  Cloud EulerOS), `openeuler`, `kylin`, `anolis`,
  `tencentos`. All use `sudo yum install -y fuse3` (single
  `fuse3` package provides `libfuse3.so.3` on RHEL 9 family;
  the previous `fuse3-libs` companion was a RHEL 8 leftover).

### Fixed
- `install.sh` `preflight_fuse` previously sourced
  `/etc/os-release` in the current shell, which clobbered
  `$VERSION` (the veda binary version) on distros that
  define their own `VERSION` field. HCE ships
  `VERSION="3.0 (x86_64)"` → next download_asset built a URL
  with a literal space → curl "URL using bad/illegal format".
  Source in a subshell, only echo `$ID` back.

## [0.1.7] — 2026-05-18

### Changed
- `veda init` is now the only auth entry point. The `login`, `claim`, and
  `account` top-level subcommands have been removed; their behavior moved
  under exclusive mode flags:
  - `veda init --upgrade --email X` (was `veda claim`)
  - `veda init --import-key vk_…|wk_…` (was `veda login --api-key`)
  - `veda init --login --email X` (unchanged)
  - anonymous and named flows unchanged
- `--import-key` automatically backs up the existing `~/.config/veda/config.toml`
  to `config.toml.bak.<unix-ts>` before overwrite, and for `vk_` keys
  resolves the server's existing `default` workspace via find-or-create
  (rather than blindly creating, which surfaced from the store as a 500
  on duplicate name).
- `install.sh` resolves the latest version automatically from the chosen
  source (GitHub `/releases/latest` API or GitLab `latest/LATEST_VERSION`
  pointer file uploaded by CI). `--from-gitlab` / `--from-github` flags
  were dropped; `VEDA_SOURCE` env var remains as the override.
- `install.sh` no longer overwrites `server_url` in an existing
  `config.toml` — only sets it when unset or at the `CliConfig::default`
  value.
- `veda cat` slice flags: `--lines A:B` removed; replaced by
  mutually-exclusive `--range A:B` / `--head N` / `--tail N` (clap
  enforces the exclusion at parse).
- `veda workspace` has a short alias: `veda ws <action>`.
- `veda cp` rejects non-UTF-8 input client-side with a path-aware
  "looks binary" / "not valid UTF-8" message (NUL-byte sniff + UTF-8
  validation) before any HTTP call.
- `skill.md` rewritten 372 → 243 lines; FUSE-specific guidance moved
  to a new `skill-fuse.md` companion (only installed by `install.sh
  --with-fuse`).

### Added
- Global `--json` flag: `veda --json ls/search/grep` emits one compact
  JSON object per line instead of the human-formatted table.
- `LATEST_VERSION` pointer file uploaded by GitLab CI's `publish:all` job
  for any non-prerelease tag (`*-test`/`*-rc*/-alpha*/-beta*` are skipped).
- `CHANGELOG.md`.
- `skill-fuse.md` — FUSE companion skill doc.

### Fixed
- `install.sh` Linux FUSE preflight reads y/N from `/dev/tty` rather than
  stdin (which is the piped script body under `curl … | sh`).

## [0.1.6] — 2026-05-15

### Added
- **FUSE write-back mode** (`--write-mode=writeback`, default still `sync`)
  with in-memory shadow buffer + 5 s debounce. vim swap files, git
  lockfiles, and IDE temp files no longer reach the server. Per-file
  10 MB / total 50 MB caps; files past the per-file cap silently
  degrade to synchronous writes. `unmount` drains pending commits.
- **Anonymous-first onboarding**: `veda init` (no flags) mints account
  + workspace + both keys in one server round-trip. `veda claim`
  upgrades an anon account to a named one. (0.1.6 → superseded in
  Unreleased: `veda claim` is now `veda init --upgrade`.)
- **Hidden FUSE summary sidecars**: every mounted directory exposes
  read-only `.abstract` (L0) and `.overview` (L1) files, server-generated.
- **Multi-workspace profiles** in CLI: `veda workspace add/switch/list/rm`,
  plus global `--workspace <alias>` override per command.
- **Darwin builds** in GitLab CI matrix: `x86_64-apple-darwin` (native)
  and `aarch64-apple-darwin` (cross-compiled from Intel mac; CLI only,
  no veda-fuse).

### Fixed
- FUSE daemon mode I/O error after fork + ssh hang on launch (macOS).
- macFUSE readdir dropped entries with `ino=0` hint.
- Multiple FUSE writeback review findings (LocalOnly preservation
  across mark_dirty/truncate_to, setattr-truncate routing through
  shadow, destroy() drain).

## [0.1.5] — 2026-05-08

### Added
- `score_type` field on `SearchHit` (`rrf` / `bm25` / `cosine`) so
  agents can avoid fusing scores across scales.
- Summary debounce window (30 s) + burst detection
  (`veda_summary_enqueue_total{burst=…}` metric).
- Structured L1 prompts with explicit `{language}` slot; multilingual
  output (English or zh-CN heuristic).

## [0.1.4] — 2026-05-07

### Added
- **Real BM25 hybrid search** via Milvus 2.5 functions: dense ANN +
  BM25 sparse, RRF-fused server-side. Replaces the substring-filter
  "fulltext" approximation.
- jieba tokenizer for Chinese BM25.
- Automatic Milvus schema migration (drop + rebuild + paginated
  ChunkSync re-enqueue) when the existing collection lacks the
  `sparse_vector` field.

## [0.1.3] — 2026-05-05

### Added
- HTTP 501 from `/v1/abstract` and `/v1/overview` when the server has
  no `[llm]` section configured (was a misleading 500 before).
- `embedding.batch_size` config + `VEDA_EMBEDDING_BATCH_SIZE` env
  override.
- `scripts/release.sh` helper for cutting versions.

### Fixed
- `/v1/collections/.../search` strips `workspace_id` from the response
  (was leaking the internal tenant id).

## [0.1.2] — 2026-05-04

### Added
- `/healthz` liveness probe (auth-bypass).
- `/v1/abstract/{path}` (L0) and `/v1/overview/{path}` (L1) as separate
  endpoints, with `Retry-After` + `Cache-Control: no-store` on 202.
- `veda --version`.
- `veda cp -r` for recursive directory upload (symlinks skipped).
- `veda grep` for literal substring search (returns `path:line:content`).
- Friendly "veda-fuse not installed" message when `veda` is run from a
  build without the fuse feature.

### Fixed
- `/v1/collections/.../search` strips the embedding `vector` field
  from results.

## [0.1.1] — 2026-05-01

### Fixed
- `skill.md` examples now match the actual CLI command syntax.

## [0.1.0] — 2026-04-30

First public alpha. CI pipeline shipped, releases published to GitHub.

### Added
- `install.sh` resolves binaries from public GitHub releases.
- GitHub Actions release matrix: `x86_64-unknown-linux-gnu`,
  `x86_64-apple-darwin` (cross-compiled on macos-14 / M1).

[Unreleased]: https://github.com/jooekong/veda/compare/0.1.8...HEAD
[0.1.8]: https://github.com/jooekong/veda/compare/0.1.7...0.1.8
[0.1.7]: https://github.com/jooekong/veda/compare/0.1.6...0.1.7
[0.1.6]: https://github.com/jooekong/veda/compare/0.1.5...0.1.6
[0.1.5]: https://github.com/jooekong/veda/compare/0.1.4...0.1.5
[0.1.4]: https://github.com/jooekong/veda/compare/0.1.3...0.1.4
[0.1.3]: https://github.com/jooekong/veda/compare/0.1.2...0.1.3
[0.1.2]: https://github.com/jooekong/veda/compare/0.1.1...0.1.2
[0.1.1]: https://github.com/jooekong/veda/compare/0.1.0...0.1.1
[0.1.0]: https://github.com/jooekong/veda/releases/tag/0.1.0
