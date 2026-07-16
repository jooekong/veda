# Changelog

All notable changes to Veda are documented here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

This project is in **alpha**: minor versions can break compatibility, on-disk
shapes, and CLI command spelling without prior notice. Pin `VEDA_VERSION` if
that matters.

## [Unreleased]

### Added
- **QA log captures the retrieval story behind each answer.** The tunnel
  records every server-announced tool call of a streamed answer — search
  queries and file reads, in execution order — into a new `tool_trace`
  column (JSON array; both DDL copies migrate idempotently). The admin
  console's Q&A details now show the asker's WeCom user id under the
  timestamp and a collapsible "过程" section listing each retrieval step.

### Fixed
- **Unrelated source lists on uncited answers.** When the answer model wrote
  no valid `[n]` marker, `/v1/answer` backfilled citations with every
  evidence block the loop had seen (initial top-12 pre-search + all
  tool-round hits). Hybrid retrieval always returns top-k, so answers with
  nothing to cite went out with a long list of unrelated sources. Ungrounded
  answers now return empty citations (`grounded` still drives the metrics
  label), and the tunnel QA-log `ungrounded` outcome — which keyed on empty
  citations and could never fire against the backfill — now classifies
  correctly.
- **Empty LLM completions now fail loudly instead of persisting.** An empty
  (post-trim) chat completion is treated as a retryable error in the LLM
  client — a summary can never legally be empty, so HTTP 200 + `content:""`
  (observed during the 2026-07 incident) now retries and, if persistent,
  dead-letters visibly instead of silently storing an empty abstract.
- **Stale SummarySync tasks on binary files no longer dead-letter.** A file
  written as text then overwritten as binary before the worker ran left an
  orphaned SummarySync that errored through all retries (315 dead tasks in
  prod). The worker now skips blob files with a warning; the blob stays
  downloadable, no summary is attempted.
- **Language detection no longer fooled by YAML frontmatter.** Summary
  output-language detection samples the document body after skipping a
  leading `--- … ---` block, so Chinese docs emitted by API-doc generators
  (ASCII-heavy frontmatter) get Chinese summaries instead of English ones.
  Existing summaries only change on the next content update or an explicit
  re-enqueue.
- **Empty L0 abstracts from reasoning-model truncation.** Summary generation
  sent `max_tokens=150` for L0; on gateway backends where a reasoning model's
  thinking tokens count against that budget, generation could exhaust it
  mid-thought and return an empty `content` with HTTP 200, which was then
  stored as a ready-but-empty abstract (hit ~10% of files in prod
  2026-07-08..13). All summary calls (file L0/L1, directory aggregate) now
  share the `max_summary_tokens` budget, default raised 2048 → 8192
  (directory aggregation was measured thinking up to ~5k tokens).

### Changed
- **`answer_max_output_tokens` default raised 1024 → 4096.** Production
  answers measure p50 ≈1.4k chars (max ≈2.6k) — the 1024 ceiling was
  silently not enforced by the LLM gateway. The new default matches real
  output so a gateway that starts enforcing `max_tokens` won't truncate
  the majority of answers.
- **WeCom answer sources render compactly.** After a `———` separator, the
  tunnel lists one `[n]` + file basename entry per line — at most 3 entries,
  the rest folded into a final "等 N 篇" line — instead of full paths for
  every citation. Entries whose basename collides within the displayed set
  fall back to their full path, so same-named files in different directories
  stay distinguishable. (Moving sources into a `<think>` block was probed
  and abandoned: WeCom stalls on a final frame carrying think tags.)
- **`veda status` no longer flags a missing account key.** The account key
  (`vk_`) is optional for data-plane use — a config holding only a pasted
  `wk_` workspace key is fully functional, yet status rendered
  `API key: ✗ missing` as if setup were broken. The line is now labeled
  `Account key`, shown only when a key is configured, and omitted otherwise.
- **`/v1/answer` is now agentic.** The LLM drives retrieval itself through
  tool calls (`search` for re-querying with different keywords, `read_file`
  for pulling full context around a hit) across up to
  `answer_max_tool_rounds` (default 4) rounds, instead of the old
  retrieve-once → assemble → single-completion pipeline. Empty retrieval no
  longer short-circuits: the model rewrites keywords and searches again; the
  canned "not found" phrase is now always model-produced. Citations with an
  empty `spans` array mean the whole file (evidence came from `read_file`).
  The SSE stream gains a `reset` event telling consumers to discard deltas
  accumulated so far (rare talk-then-tool-call rounds); the `final` frame
  stays authoritative. Route deadline 45s → 90s. Config: `[llm]`
  `answer_max_tool_rounds` added, `answer_max_context_tokens` removed.

### Added
- **`GET /v1/whoami` — data-plane identity probe.** Resolves the presented
  `wk_` (either kind, no kind gate) to `{workspace_id, kind, permission}`.
  `veda status` and `veda init --import-key wk_…` use it to backfill the
  workspace id a pasted key can't carry, so status shows a real id instead
  of `(id unknown)`. Best-effort against servers predating the endpoint:
  404 leaves the config untouched and status retries next run.
- **Per-request answer persona: `prompt` field on `/v1/answer(/stream)`.**
  Appended to the built-in knowledge-base protocol (tool policy, citation
  rules, injection guard — not overridable), ≤4000 chars; absent falls back
  to the server default persona. Groundwork for per-bot prompts in the WeCom
  tunnel.
- **Built-in Console file manager.** fs workspaces now expose a `Files` entry
  in `#/console`; it lists directories, uploads a selected text or binary file
  and downloads original bytes. The workspace `wk_` is kept only in the active
  browser tab and sent directly to the native `/v1/fs/*` data plane.
- **Tool-call progress in streaming answers.** `POST /v1/answer/stream` gains a
  fifth SSE event `tool` `{"name","detail"}`, emitted right before each tool
  call runs (search → the query, read_file → the path, char-capped, never tool
  results). The WeCom tunnel renders it as a status line in the reply bubble
  (「🔍 正在检索:…」/「📄 正在查阅:…」), covering the silent stretch between
  the placeholder and the first answer delta. Older consumers ignore unknown
  events — purely additive.
- **Platform QA telemetry reads for WeCom tunnel bots.** The apps surface adds
  `GET /v1/workspace/{workspace}/project/{id}/tunnel/qa/stats` (outcome
  distribution + thumb up/down over a `days` window, clamped 1–90) and
  `.../tunnel/qa/logs` (paginated Q&A detail, filterable by `outcome` /
  `down_voted` / `bot_id`). Both read the tunnel's shared `veda_tunnel_qa_log` /
  `veda_tunnel_qa_feedback` tables scoped to the project's own bots — a `bot_id`
  outside the project collapses to `NOT_FOUND`, and a project with no bots
  returns empty. fs-project only; reads go through external authz (the detail
  carries user questions and bot answers).

## [0.1.17] — 2026-07-13

### Added
- **`POST /v1/answer/stream` — streaming RAG answers (SSE).** Same request
  as `/v1/answer`; the response streams `delta` events (incremental LLM
  text) and ends with an authoritative `final` frame carrying the full
  `AnswerApiResponse` (citations align only against complete text — always
  replace accumulated deltas with it); failures after the 200 arrive as an
  `error` event. Pre-checks (400/401/429/501) stay plain HTTP. Mounted
  outside the 30s TimeoutLayer with a 45s per-event guard; the per-workspace
  concurrency permit spans the whole stream. veda-tunnel consumes it with
  ≥1s-throttled WeCom bubble refreshes (first token visible in ~1-2s instead
  of a 7-11s blank wait) and falls back to the one-shot endpoint on older
  servers. Plan: `docs/plans/veda-answer-stream.md`.
- **veda-tunnel QA telemetry (qa_log).** Every WeCom Q&A lands a row in
  `veda_tunnel_qa_log` (query, full answer text, outcome, latency, citation
  count; best-effort — never blocks the reply), and each reply's first stream
  frame carries `feedback.id`, activating WeCom's thumb-up/down UI; votes
  flow back via `feedback_event` into `veda_tunnel_qa_feedback` (re-voting
  replaces). `no_context` classification matches the server's canned-refusal
  prefix (semantic retrieval always returns top-k, so hit-count can't signal
  it) — that list doubles as the knowledge base's missing-docs backlog. New
  admin endpoints `/admin/stats` + `/admin/qa-log`; the console tunnel page
  gains stat cards and a filterable Q&A/bad-case table. Admin bot writes now
  leave audit log lines. Plan: `docs/plans/veda-tunnel-qa-log.md`.
- **Platform fs file upload/download.** The AI Workbench data plane gains
  `PUT /v1/workspace/{ws}/project/{id}/file?path=` (raw-byte body, same
  UTF-8-vs-blob content sniff as the `wk_` plane, parents auto-created,
  overwrite bumps revision, 50MB quota) and
  `GET .../file/content?path=` (raw byte stream with stored MIME +
  RFC 5987 attachment filename). Both behind external authz; fs-only.
  Contract: APIDoc `docs/veda/fs-data-api.md` §6–7.
- **Platform tunnel-bot API — attach a WeCom bot to an fs project from the AI
  Workbench.** New apps-surface CRUD
  `/v1/workspace/{ws}/project/{id}/tunnel/bots` (fs-only; gateway authz +
  company envelope): create auto-mints a dedicated read-only `wk_` for the
  project (revoked again on delete — a conflicting create rolls its key back
  too), secret is write-only, responses carry tunnel's live `conn_state`
  heartbeat. veda-server writes the `veda_tunnel_bots` table shared with
  veda-tunnel (both sides run the same idempotent CREATE+ALTER schema
  bootstrap, so deploy order is free); the tunnel process converges within
  one 30s store poll — no RPC between the two. Contract: APIDoc
  `docs/veda/tunnel-bot-api.md`.
- **veda-tunnel store-poll reconcile + heartbeat.** The control loop now
  re-reads the bot table every 30s and diffs desired vs running
  (`reconcile::plan`: spawn new / respawn changed / stop removed), so rows
  written by the platform API take effect without a restart; it also writes
  each bot's connection state back (`conn_state`/`conn_updated_at`, on
  change only) for the platform API to display. Deployed to the dedicated
  production box 10.79.52.95 (`docs/deploy-tunnel.md`).
- **`POST /v1/answer` — RAG knowledge-base Q&A (P0, fs workspaces).** Retrieval
  → tiered context assembly → LLM answer with verifiable citations
  (`citations[{index,path,spans}]` map each inline `[n]` to the exact chunk
  range backing it). Assembly: neighbor-window merge with ellipsis markers,
  per-doc cap, index-freshness watermark guard, Ready-only L0 abstracts, and
  post-expansion whole-span budget trimming. Route runs outside the 30s
  TimeoutLayer with its own 45s deadline, per-workspace concurrency gate
  (429), 501+`no-store` when `[llm]` is absent, explicit 502/504 mapping, and
  never calls the LLM on empty recall. veda-tunnel routes questions through it
  by default (`[answer] enabled`, falls back to raw search when off). Verified
  end-to-end against real MySQL/Milvus/airouter: grounded cited answers,
  refusal (not hallucination) on out-of-scope questions. Design + review log:
  `docs/plans/veda-answer-plan.md`.
- **veda-tunnel — external IM bridge (new crate, scaffold).** An independent
  process + binary (`crates/veda-tunnel`) that brings veda retrieval into
  WeCom (企业微信) via the aibot long connection (WSS). A standard `wk_`
  consumer of the data plane — veda-server is untouched. One bot = one long
  connection = one read-only `wk_` (one workspace): an `@`-mention/DM is
  stripped, sent to `POST /v1/search`, and the top hits (content + source
  `path`) are streamed back (a `finish:false` placeholder absorbs WeCom's 5s
  deadline, `finish:true` delivers results). Fail-closed admin surface
  (`GET /admin/bots`, `reconnect`, `reload`, `/healthz`; `admin_token` unset →
  404) on `127.0.0.1:9100`. Single instance by design (WeCom's
  new-kicks-old single-connection rule). **Bots are managed at runtime** — stored
  in MySQL (`veda_tunnel_bots`, same instance as veda; `config.toml` bots are a
  first-run seed only), CRUD'd via a fail-closed admin API
  (`GET/POST/PUT/DELETE /admin/bots`; secret never returned, `veda_key` masked,
  edit keeps secret/key when left blank) that spawns/stops connections
  dynamically without a restart, plus a management page in the veda web admin
  console (`#/admin/tunnel`, reached through an nginx `/tunnel/v1/*` reverse
  proxy). **20 unit tests pass; live-bot + real-MySQL CRUD verified 2026-07-09**
  (subscribe/30s heartbeat/hot key-swap + fs `wk_` @-mention→search→streamed
  reply end-to-end). Design: `docs/plans/veda-tunnel-plan.md`.
- **Platform gateway surface (AI Workbench).** A new API family under
  `/v1/workspace/{workspace}/...` lets the company AI platform embed veda with
  auth externalized to the platform gateway (identity via a base64 `user`
  header + cookie-forwarded external authz; no veda credential). Control
  plane: project (= veda workspace) / dataset / key lifecycle, plus
  `GET /v1/my/projects` (the gateway user's projects, flattened, keyword
  filter on name/description, offset pagination). Data plane:
  `/project/{id}/vectors/{upsert,search,query,delete}` (db projects) and
  `/project/{id}/{search,files,file,sql,grep}` (fs projects) — every op,
  read and write, passes the external authz check. Responses are rewritten
  to the company envelope (`{data:[...], page,...}` for lists). File preview
  returns `is_binary` + a localized "preview unavailable" notice for binary
  files instead of mojibake; `files` listing now reports real
  `mime_type`/`size_bytes`.
- **Read-only admin dashboard + db vector write console** under
  `/admin/v1/...` (frontend in `web/`), gated by a dedicated `admin_token`
  (fail-closed: 404 when unset). Cross-tenant workspace/file browsing,
  vector search, and a manual vector upsert console for ops.
- **Binary blob storage + PDF text extraction.** `PUT /v1/fs/{path}` now sniffs
  the body: valid UTF-8 stays on the existing text path (LONGTEXT, chunked,
  grep/SQL/line reads — unchanged); non-UTF-8 is stored verbatim as a blob in a
  new `veda_file_blobs` (LONGBLOB) table with `storage_type=blob` and a real
  `mime_type` detected from magic bytes (`infer`). PDFs additionally enqueue an
  `ExtractSync` outbox event — a worker extracts the text layer (`pdf-extract`,
  pure-Rust) and embeds it into Milvus, so the original PDF stays downloadable
  byte-for-byte while its content becomes searchable. Images / jars / other
  binaries are stored but not indexed. `GET /v1/fs/{path}` returns the raw bytes
  with the real `Content-Type`; byte-range reads work on blobs, line reads
  reject them. The new table is created via `CREATE TABLE IF NOT EXISTS` (no
  migration step). Adds deps `infer` + `pdf-extract`.

### Changed
- **CLI Linux binary is now static musl.** The release `veda` CLI for Linux
  ships as `x86_64-unknown-linux-musl` (statically linked) instead of `…-gnu`,
  so it runs on any Linux regardless of host glibc version — no more
  `version 'GLIBC_2.xx' not found` on older boxes (e.g. the glibc-2.34 alpha
  box). `veda-fuse` stays gnu (it dynamically links libfuse3). `install.sh`
  resolves the musl CLI artifact automatically; on Linux the binary names now
  diverge (`veda-…-musl` vs `veda-fuse-…-gnu`).
- **CLI (`veda`) now handles binary.** `veda cp` uploads raw bytes for both text
  and binary (the old client-side "looks binary" rejection is gone) — PDFs /
  images / jars upload as-is and the server decides text-vs-blob. `veda cat`
  writes raw bytes for whole-file reads (binary round-trips losslessly when
  redirected to a file) and errors clearly instead of garbling on
  `--head/--tail/--range` over a binary file. **Compatibility**: `cp`/`cat` of
  binary needs a server at this version or newer — `cp` of a binary against an
  older server returns 400; text usage is unchanged and back-compatible.
  `veda-fuse` needs no change (it already sent raw bytes).
- **Drift reconcile is now on-demand, not a 6h background loop.** The periodic
  reconciler (`[reconciler]` config, `VEDA_RECONCILER_*`) is removed. Rationale:
  the file write and its ChunkSync/SummarySync enqueue commit in one MySQL
  transaction, so the write path can't drift; the only residual drift is
  dead-letter tasks and Milvus-side data loss. Reconcile is now triggered by
  `POST /admin/v1/reconcile/{workspace_id}?dry_run=true|false` (gated by the ops
  `metrics_token`; default `dry_run=true` reports only). Failures (e.g. the
  Milvus 16384-window list cliff on a very large workspace) surface as a 500 to
  the operator instead of a silent per-workspace skip. `[reconciler]` keys in an
  existing config are ignored.
- **BREAKING (db data-plane auth)**: `/v1/vectors/*` now authenticates with a
  workspace key `wk_` (`AuthDbWorkspace`), not the account key `vk_`. A `wk_` is
  bound to one workspace, so the request body no longer accepts `workspace_id`
  (dropped from upsert/search/query/delete). A read-only `wk_` may search/query
  but not upsert/delete. The account `vk_` is control-plane only now. Existing
  db workspaces must mint a `wk_` to keep using the data plane; the Java SDK
  must switch `vk_`→`wk_` (not yet done).
- **Auth is one MySQL query per request now**: the `wk_` lookup JOINs
  `veda_accounts` to check account status (`veda_workspace_keys` denormalizes
  `kind`/`account_id`) and no longer reads `workspace.status` — workspace
  archive instead cascade-revokes all its `wk_` in the same transaction, so
  data-plane calls after archive fail **401**. The `allowed_workspaces` scope
  of a `vk_` is enforced at the single ownership gate (`load_owned_workspace`),
  closing the scoped-`vk_` → out-of-scope `wk_` minting hole (S1).
- **Wire (additive):** `/v1/vectors/search` hits include a new `score_type`
  field. Deserializes with a `cosine` default, so older payloads keep their
  (semantic-only) meaning; SDK/clients should surface it.
- **Default search mode is now `hybrid`** (was implicitly semantic). `hybrid`
  failures surface as errors — no silent fallback to semantic.

### Fixed
- **FUSE: `rm -rf` no longer trips over summary sidecars.** Deleting a
  directory used to error with `rm: dir/.abstract: Read-only file system`
  and leave the directory shell behind, because unlink on the synthetic
  `.abstract` / `.overview` entries returned EROFS. unlink on a sidecar is
  now a no-op success (the summary lives as long as its directory; it
  reappears on the next lookup), so recursive deletes finish cleanly and
  the directory itself is removed. Sidecars stay read-only otherwise:
  write / create / rename onto them still fail, and `rmdir` on one now
  returns the POSIX-accurate ENOTDIR instead of EROFS.
- **Outbox lease fencing (A-3)**: workers on multiple servers can now safely
  share one MySQL. `claim()` stamps a `lease_owner` (`host:pid`); complete /
  fail / renew are fenced on `owner + status='processing'`, so a stale executor
  whose lease was taken over can no longer overwrite the new owner's task
  state. A 3-min batch heartbeat keeps slow-but-healthy tasks inside the 10-min
  lease; `veda_outbox_lease_lost_total{op}` / `veda_outbox_lease_takeover_total`
  surface lost leases and takeovers. Mixed-version caveat: fencing fully
  protects only once all workers sharing the MySQL run the new binary.
- **Graceful shutdown actually runs in production**: worker shutdown drains the
  in-flight batch instead of cancelling it at an await point, and the server
  now listens for SIGTERM (systemd stop only sends SIGTERM; the old
  ctrl_c-only handler never fired).
- Hot-path indexes (A-8): `veda_dentries (workspace_id, file_id)` and
  `veda_outbox (workspace_id, event_type, status)`.

### Removed
- **BREAKING**: JWT workspace tokens. `POST /v1/workspaces/{id}/token` is gone,
  `AuthWorkspace` (fs data plane) now accepts only `wk_`, and `jwt_secret` config
  is dropped. All auth is a plain key check (no JWT mint/verify).

### Added
- **Dead-letter visibility (H4)**: `veda_outbox_dead_total{event_type}` counter
  emitted at both death sites — the `fail()` retries-exhausted path and the
  previously-silent `claim()` lease-expiry path — plus a `veda_outbox_depth{status}`
  gauge (pending/processing/dead, sampled every 30s). Lets ops alert on
  permanently-failing tasks and queue backlog (configure the alert rule on the
  Monitor platform). Closes the alpha-plan "outbox_depth + outbox_dead 可见" gap.
- **On-demand reconcile endpoint**: `POST /admin/v1/reconcile/{workspace_id}`
  (see Changed). `?dry_run=true` logs/returns the drift report without mutating;
  `?dry_run=false` enqueues repairs and deletes orphans.
- **Platform account model**: accounts can be created by `app_id` —
  `POST /v1/accounts {name, app_id}` with no email/password — so the AI platform
  provisions one veda account per business app. `Account` gains a unique `app_id`;
  the `vk_` is returned once and the platform keeps it (no email login, no v0
  re-issue path). Email+password creation (console/CLI) still works;
  `CreateAccountRequest` email/password are now optional. `POST /v1/accounts`
  stays public in v0 — **trusted-network only** (any caller can squat an
  `app_id`); add a platform credential before any public exposure. app_id
  accounts are passwordless and cannot be `claim`ed into email/password login.
- Workspace key lifecycle: `GET /v1/workspaces/{id}/keys` (list — metadata only,
  never the plaintext) and `DELETE /v1/workspaces/{id}/keys/{key_id}` (revoke),
  for managing keys from the console / AI platform.
- `workspace` and `dataset` creation accept an optional `description`.
- db-workspace `/v1/vectors/search` now supports `mode`: `hybrid` (dense +
  BM25 fused by RRF), `semantic` (dense ANN), and `fulltext` (BM25 only,
  skips embedding). The per-collection sparse/BM25 schema was already built;
  this lights up the query path. Verified against real Milvus 2.6.14.
- `VectorSearchHit` gains `score_type` (`cosine` / `bm25` / `rrf`) so callers
  can tell which ranker produced `score` — the three are not comparable.
- `/v1/vectors/search` accepts an optional `min_score` relevance floor (drops
  hits below it). Valid only for `semantic` / `fulltext`; rejected with `400`
  for `hybrid` (incl. the default), whose RRF score is a rank artifact, not a
  relevance value. Applied after `top_k`, so the result may be shorter.
- **Java SDK** (`sdk/java`, `csoss.veda:veda-sdk-java`): hand-written Java 8 client
  for the db-workspace vector data-plane (`upsert`/`search`/`query`/`delete`).
  Jackson + OkHttp, fluent filter builder, `error_code` → typed exceptions with
  `UNKNOWN` fallback, idempotency-aware retry (id-less upsert is never
  auto-retried), and forward-compatible deserialization. Builds + unit-tests in
  CI; real-server contract tests run via `mvn -P integration verify`.
  Released `0.0.1-RELEASE` to the internal ddxq Nexus (2026-06-17; the `0.0.1-SNAPSHOT`
  preview shipped 2026-06-04). Verified against the `wk_` data-plane — full
  integration suite green on the live server (28 unit + `VedaClientIT` 5 +
  `VedaE2EIT` 4).
- `/v1/vectors/upsert` accepts `write_mode`: `upsert` (default, idempotent
  dedup-by-id) or `insert` (skips Milvus's dedup+delete for ~3× write
  throughput; caller guarantees `id` uniqueness — a repeated id inserts a
  duplicate row).
- **OTLP metrics bridge**: a background task pushes all metrics to the company
  Monitor Collector every 5s over OTLP gRPC (counter→Sum, gauge→Gauge,
  histogram→bucket-delta; `[otlp]` config + `VEDA_OTLP_*` env gates, off by
  default). Vendored proto provenance: `crates/veda-server/proto/PROVENANCE.md`.
- **Vector data-plane metrics**: `veda_vector_request_seconds` (handler),
  `veda_vector_store_op_seconds` (store), `veda_milvus_request_seconds` (Milvus
  HTTP), labeled by operation / workspace_id / dataset / mode / outcome.

## [0.1.13] — 2026-05-22

### Fixed
- Stale directory summaries on four paths — embed-fail, delete, rename, and
  empty-children — plus a dir-summary enqueue dedup race.

## [0.1.12] — 2026-05-20

### Added
- `veda-fuse mount` falls back to the CLI config (`~/.config/veda/config.toml`)
  for `--server` / `--key`, and gains `--workspace <alias>` to pick a CLI
  workspace profile. The web console shows a ready-to-paste mount command.

### Fixed
- FUSE sync mode: write handles re-base after a `setattr` truncate, so
  truncate-then-write no longer resurrects stale bytes.

## [0.1.11] — 2026-05-20

### Added
- Release artifacts for `aarch64-apple-darwin` now include `veda-fuse` —
  Apple Silicon no longer needs a source build for the FUSE mount.

## [0.1.10] — 2026-05-20

### Added
- Web frontend (`web/`, Vite + TS + Tailwind): landing page, zh/en i18n, and
  the user docs site served under `/docs`.

### Changed
- FUSE attribute caching: TTL split into `--attr-ttl` (default 30s) and
  `--dir-ttl` (default 60s), plus SSE cache-consistency fixes.

## [0.1.9] — 2026-05-19

### Added
- `veda search --path <prefix>` — restrict a search to a workspace
  subtree (e.g. `--path /docs`). Server already accepted `path_prefix`
  on `SearchApiRequest`; this just plumbs it through the CLI.

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

[Unreleased]: https://github.com/jooekong/veda/compare/0.1.13...HEAD
[0.1.13]: https://github.com/jooekong/veda/compare/0.1.12...0.1.13
[0.1.12]: https://github.com/jooekong/veda/compare/0.1.11...0.1.12
[0.1.11]: https://github.com/jooekong/veda/compare/0.1.10...0.1.11
[0.1.10]: https://github.com/jooekong/veda/compare/0.1.9...0.1.10
[0.1.9]: https://github.com/jooekong/veda/compare/0.1.8...0.1.9
[0.1.8]: https://github.com/jooekong/veda/compare/0.1.7...0.1.8
[0.1.7]: https://github.com/jooekong/veda/compare/0.1.6...0.1.7
[0.1.6]: https://github.com/jooekong/veda/compare/0.1.5...0.1.6
[0.1.5]: https://github.com/jooekong/veda/compare/0.1.4...0.1.5
[0.1.4]: https://github.com/jooekong/veda/compare/0.1.3...0.1.4
[0.1.3]: https://github.com/jooekong/veda/compare/0.1.2...0.1.3
[0.1.2]: https://github.com/jooekong/veda/compare/0.1.1...0.1.2
[0.1.1]: https://github.com/jooekong/veda/compare/0.1.0...0.1.1
[0.1.0]: https://github.com/jooekong/veda/releases/tag/0.1.0
