# Prune Audit Verification — 2026-08-18

Verification of the 31-finding prune audit ("Veda Prune Ledger", produced 2026-08-18
against HEAD `d8a0ed0`, 9 crates / 74,846 LOC). Verified independently by two passes:

- **Claude**: 4 parallel read-only agents (reported-tier re-derivation, swallow/knob
  checks, large-cut trap hunt, decision-tier fact check) + direct main-thread reads of
  both bug sites; baseline `cargo check --workspace --all-targets` re-run clean.
- **Codex**: full adversarial pass over all 31 findings (session
  `01a013c4-41e1-70e1-b263-f28c1a03959b`).

Both passes agree on the headline: **the report is high quality but must NOT be applied
as one batch.** Its own evidence tiers predicted where the errors were — the
compiler-verified tier held 100%; every refutation sits in the grep/reported/your-call
tiers.

## Scorecard

| Tier | Findings | Held up | Refuted / materially corrected |
|---|---|---|---|
| Bugs (01–02) | 2 | 2 (01 severity nuanced) | — |
| Free wins (03–09) | 7 | 7 | — |
| Dead surface (10–26) | 17 | 13 | 16a, 25d refuted; 24a/24e downgraded; 15/19/21/25f/25h corrected |
| Your call (27–31) | 5 | 2 | 28d, 30b(Derived), 31b refuted/stale |

## Bugs — both real, apply first

- **01 append truncates on missing content row** (`fs.rs:1910` vs sibling fast-fail
  `fs.rs:563`): confirmed. Nuance (Codex): the trigger is an already-corrupt/missing
  content row — not producible by normal API writes — so it is a latent
  data-destruction amplifier, not a reachable bug in healthy operation. **Missed twin
  (Codex): `veda-server/src/worker.rs:260-264` `load_full_content` has the identical
  `unwrap_or_default()`** — a corrupt row silently empties chunk/embedding input. Fix
  both together, fail-loud, with regressions for append and worker.
- **02 fs hybrid silent ANN fallback** (`milvus.rs:1129-1137`, fallback `:1278-1282`):
  confirmed by everyone; contradicts the repo's own D4 decision (`milvus.rs:861-865`)
  and field-guide landmine #2. Compound cut confirmed: the fallback is the only `Some`
  caller of `ann_search`'s `text_filter` (`:1281`), so the param + LIKE-escape block
  die with it. Regression: forced hybrid endpoint error must surface `Err`, never
  `score_type=cosine`.

## Refuted — do NOT apply as written

| # | Claim | Why it's wrong |
|---|---|---|
| 16a | `CollectionType::Raw` deletable | **The KeyStatus-shape trap the report warned about.** Live wire value: `CreateCollectionRequest.collection_type` (`api.rs:333-336`) → `POST /v1/collections` accepts `"raw"` today; `remote_e2e_test.rs:1174-1188` posts it against prod. Persists to `VARCHAR(16)`, read back via `db_enum` → one `'raw'` row poisons `list_collections` for the whole workspace. Safe path: 400 at the route + `SELECT DISTINCT collection_type` sweep first; delete the variant only after both. |
| 25d | `FileFormat::Tsv` foldable into `Csv{b'\t'}` | `read_glob_files` guards mixed-format globs via `std::mem::discriminant` (`fs_table.rs:166`) — folding merges the discriminants, so CSV+TSV globs bypass rejection. Only possible with a guard rewrite to full equality + regression; not worth it. |
| 30b | `MemoryKind::Derived` dead | Wire-reachable: `SaveMemoryApiRequest.kind` (`api.rs:553`) deserializes `{"kind":"derived"}` via REST (`routes/memory.rs:54`) and MCP (`mcp.rs:1046`) and persists. Bonus gap: no read-side kind discount exists (design doc's 读侧按 kind 打折 unimplemented), so injected `derived` ranks like `fact`. Also (Codex): the report's `SELECT DISTINCT` gate cannot detect `scope:"self"` clients — it folds to `mine` before persist; retiring `"self"` needs client-traffic knowledge or a deprecation cycle. |
| 31b | `deploy/systemd/` orphaned fork | Premise stale, direction inverted: the 2026-06-18 reviews flagged it as the *hardened* unit to adopt; that landed — `scripts/deploy/veda-server.service` now carries the hardening. And it is not orphaned: `docs/deploy.md:72-73,124-128` documents it as the non-socket-activation variant and cites its env example. Keep. |
| 28d | snippet path reaches only `search_reply` | `BotConfig.mode`/`limit` have five other readers incl. the **web console bot editor** (`web/src/admin.ts:1118-1124` mode select + limit input), store INSERT/UPDATE, validator, admin DTO; `outcome:"raw_search"` is in the console badge map + server whitelist. Cutting #28 strands console UI + DB columns; churn is ~168 dedicated lines + ~19 threading sites, above the ~150 estimate. Product decision, not residue. |

## Downgraded / corrected (direction still right)

- **24a `search_summaries` `_ => Ok(vec![])`**: caller always embeds and errors on an
  empty batch (`search.rs:242-248`), but trait-level `query_vector` is `Option` and a
  zero-length vector still lands in `_`. Fix = make the arm an `InvalidInput` error;
  "unreachable → delete" was too strong.
- **24e memory `source_ref` `unwrap_or(0)`**: not a bug — serializing a
  `serde_json::Value` is infallible, and the claimed "sibling validator that does it
  correctly" does not exist. Simplify only.
- **24d `count_index_backlog` `_ => {}`** (Codex): schema is unconstrained VARCHAR +
  ci collation can pass `IN` yet miss Rust literals — make it an error, don't call it
  unreachable.
- **15 `upsert_chunks`**: cut is right but cascade understated —
  `upsert_chunks_only`'s **default trait body calls `upsert_chunks`**
  (`store.rs:513-515`), so it must become a required method and 4 mocks +
  `milvus_test.rs:78` reworked.
- **19 text/plain extractor arms**: unreachability holds, but there are 3 ExtractSync
  producers (fs.rs, reconciler.rs:275, backfill-word-extracts.sql), not 1 — all gate
  on pdf/word. OLE arm confirmed load-bearing.
- **25f fuse `nlookup_count`**: already `#[cfg(test)]`-gated — not a finding.
- **25h MCP header**: 12 tools, **3** writers (not 4); `memory_context` is read-only.
  Extra doc debt: `web/public/docs/{en,zh}/skill.md` still claim all MCP tools are
  read-only.
- **Counts**: 22b has 12 callers (not 13), 22c has 18 (not 19); 16c's live
  `MemoryScopeType::parse` caller is `veda-server/src/worker.rs:824` (not
  veda-pipeline).

## Confirmed as claimed (spot highlights)

03 (deps; 3 riskiest re-checked statically), 04 (stale 47KB lock), 05, 06 (the
`account.app_id` read at `routes/account.rs:313` is a DB row, not the extractor
field), 07, 08, 09, 10 (incl. no scrape/alert consumers of `veda_drift_total`;
obs bridge is generic), 11 (note: removing the `CREATE TABLE` index line is cosmetic
for existing DBs — a real drop needs an idempotent `DROP INDEX` migration), 12
(+ bonus dead re-export `mysql/mod.rs:216`), 13 (keep `request_dimensions` — live),
14 (SQL backfills insert without `id`), 17 (two sites are match guards — slightly
bigger edit), 18, 20, 21 (+ 5th body `query_fulltext` at `milvus.rs:1168`; summary
path genuinely uses `id` — don't touch), 22a/d, 23a–c (23d handlers add the leading
slash — move `format!` into callee if merging), 26a/b (fast-fail per house style;
both are read-path behavior changes — deliberate yes required), 27a–f (dates
verified; deleting the DROP shifts the rollback hazard direction rather than
eliminating it; tunnel DDL is duplicated by design in veda-tunnel — prune both
sides), 29a (~79 lines; `StreamUnsupported` shares match arms — they shrink, not
vanish), 29c (exactly 137 test lines; only consumer is users' on-disk config),
30a (M2a did not change SelfScope; divergence deferred to M3), 31a (facts hold;
unused today but it is the only immediate full-convergence control after
out-of-band DB edits — keep-or-cut is an ops call), 31c (making `memory`
non-optional forces MySQL+Milvus into 4 unit tests — keep the `Option`).

## Apply plan

1. **Bugs, independent of pruning**: 02 (+ structural test), then 01 + worker twin
   (+ corrupt-inline-row regressions for append and worker).
2. **Mechanical batch** (compile-gated, one commit): 03, 04, 05, 06, 07, 08, 09, 12
   (+ dead re-export), 14, 17, 18, 20, 22a/b/c/d, 23a–c, 25a/b/c/e/g + both header
   fixes (25h incl. skill.md), 10, 13 (prod-path only), 19 (arms only).
3. **Behavior-adjacent, each a deliberate yes**: 24a/24b/24c/24d as fail-loud fixes,
   26a, 26b, 21 (needs live-Milvus gate), 15 (trait surgery), 16b + 16c (after
   `SELECT DISTINCT status` sweep), 11 (code+docs now; `DROP INDEX` as separate
   migration decision).
4. **Joe-only decisions, not cleanup**: 27 (rollback-window semantics), 28 (product
   capability + console surface), 29a/b (compat posture), 30 (wire vocabulary;
   Derived needs a policy — reject at save or implement read-side discount), 31a
   (ops semantics). 16a and 31b: **do not delete**; for 16a optionally add
   route-level 400 + DB sweep.

## Verification gates

```bash
cargo fmt --check
cargo check --workspace --all-targets
cargo test --workspace
cargo test -p veda-store --test milvus_test -- --ignored --test-threads=1  # live Milvus, for 21/15
```

Pre-prod DB gates (before 16b/27/30 decisions):

```sql
SELECT collection_type, status, COUNT(*) FROM veda_collection_schemas GROUP BY collection_type, status;
SELECT DISTINCT kind FROM veda_memories;
SELECT DISTINCT kind, source FROM veda_principals;
SELECT COUNT(*) FROM veda_workspace_keys WHERE account_id = '';
SHOW INDEX FROM veda_files WHERE Key_name = 'idx_checksum';
SHOW COLUMNS FROM veda_outbox LIKE 'lease_owner';
```

## Execution log — 2026-08-18

Phases 1–3 executed same day (Opus 5 implemented per batch, Fable verified each
diff + gates, committed after acceptance). Commits `099a4dd` (bug 02), `a2a3caa`
(bug 01 + worker twin), `6572121` (2A manifests/micro), `8abfab7` (2B
store/trait), `612c174` (2C pipeline/misc), `7c1b5a0` (3A fail-loud), `e60418a`
(3B store surface). Net −2,712 lines. Phase 4 untouched, as planned.

Deviations from the plan, with reasons:

- **25c skipped**: deleting `MysqlStore::new` trades a 1-line convenience
  constructor for `PoolConfig::default()` boilerplate at ~30 test sites —
  negative value.
- **24e skipped**: `source_ref`'s `unwrap_or(0)` is unreachable (serializing a
  `serde_json::Value` cannot fail) — churn without payoff.
- **Extra fix beyond plan**: `veda-sql/src/search_table.rs` had the identical
  warn-and-continue swallow as 26a on the SQL `veda_search` UDTF path (audit
  missed it); fixed in `7c1b5a0`.
- **metrics_test.rs kept**: the plan said delete the run_once test fn, but it
  also asserts three unrelated metrics — only the circular
  run_once/veda_drift_total parts were removed.
- **Known-simplification**: worker-side bug-01 fix has no dedicated unit test
  (identical one-line pattern as the fs.rs site, which is mutation-checked);
  building a worker mock harness wasn't worth it.

Test-env deployment (prod .85 NOT deployed):

- Binary `f8c4a4ea88a2…` built on .89 from `e60418a` (glibc 2.34, sha-change
  verified), swap-first deployed to .161 + .89. Both: healthz ok, mysql+milvus
  ready, blob probe roundtrip ok, hybrid search 200, MCP self-reports 0.1.27
  (sha deploy — binary hash is the version judge, per runbook).
- DB gates on test MySQL (vecfs): `collection_type` = {raw, structured} — live
  `raw` rows exist, vindicating the 16a refutation (do NOT delete the variant);
  `status` = {active} only, so 16b was safe; `kind` = {fact}.
- Suites against the deployed env: mysql_test 33/33, milvus_test 16/16 (incl.
  new `fs_hybrid_surfaces_error`, reworked `upsert_chunks_only` write path,
  trimmed outputFields on real Milvus), the 3 config-gated local tests 3/3,
  remote e2e black-box 33/33 via the public entry.
- The `idx_checksum` index still exists on live DBs (CREATE TABLE IF NOT
  EXISTS); `SHOW INDEX` + `DROP INDEX` remains the deferred ops decision.

Production deployment (same day, after test-env verification):

- Same binary `f8c4a4ea88a2…` swap-first deployed to .85. Pre-swap DB gates
  through an SSH tunnel via .85 (prod MySQL allowlists client IPs; the Mac
  isn't on it): `status` = {active} only (16b safe), `collection_type` = {raw}
  — prod also carries live raw rows, re-vindicating the 16a refutation;
  `veda_memories` empty; outbox baseline 83 completed / 0 pending / 0 dead.
- Post-swap: healthz ok, mysql+milvus ready, disk sha matches, blob probe
  roundtrip + cleanup, hybrid search 200, MCP self-reports 0.1.27, RSS ~32MB,
  zero journal errors, outbox 85 completed (+2 = the probe's own events) /
  0 dead. All three nodes (.161/.89/.85) now run `f8c4a4ea` @ `e60418a`.

## Expected payoff

- Phases 1–3: two real bug fixes (one data-destruction class, one silent quality
  degrade), ~500–600 production LOC removed, the 1,875-line stale `Cargo.lock`,
  ~10KB/query Milvus payload at limit=100, one dead index write per file insert,
  five observability swallows converted to fail-loud, and doc truth restored
  (dedup claims in ARCHITECTURE/README/web reference, MCP tool counts, false doc
  comments). On 75k LOC the LOC delta is ~1% — the value is concentrated in the
  bugs, the trap discoveries, and the misleading-surface removals, not raw line
  count.
- Phase 4 upside if approved: roughly another 400–500 LOC (27/28/29) plus ~137
  test lines (29c).
