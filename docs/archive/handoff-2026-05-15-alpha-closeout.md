# Alpha Closeout Handoff — 2026-05-15

Joe is calling Veda alpha **functionally complete**. This handoff freezes the punchlist and tells the next session exactly what's left vs. explicitly out-of-scope.

## State of the tree

- HEAD: `13edb01 fix(fuse): preserve LocalOnly across mark_dirty/truncate_to`
- Pushed to both `origin` (GitHub) and `ddxq` (GitLab)
- Tests: `cargo test -p veda-fuse --lib` → 87 passing
- Prod server: 0.0.12-test on `10.79.51.161`, systemd-native (see `reference_prod_box.md` memory)
- Local FUSE mount: `/Users/konglingqiao/veda-mnt`, `--write-mode=writeback`, PID was 73139 at handoff time (will have changed by next session)

## Today's session shipped

The entire **FUSE write-back + debounce + cancel-on-unlink** subsystem — the largest item that had been sitting in alpha-plan §6 "后续路线":

- Plan: `docs/plans/fuse-writeback-plan.md`
- 5-day implementation: commits `d15d3f6` (Day 1 shadow) → `1fb6f29` (Day 5 destroy drain)
- Each day reviewed by Codex; review fixes in `8fd0a7c`, `0a4d32a`, `9a3d472`, `b9018ea`, `3b49dca`
- Setattr-truncate writeback bug discovered + fixed: `3530d66`
- `/simplify` pass + LocalOnly preservation: `5b9cecf`, `13edb01`
- Triggered by Joe hitting vim's `E72: Close error on swap file`; resolved end-to-end (vim swap, git lockfile, IDE temp files now produce zero server calls)
- 87 unit tests pass; real-mount smoke (create / overwrite / truncate-extend) all clean

## Remaining alpha-soft work

The only two things left before tryout:

### 1. S8 — outbox retention sweep

`veda_outbox` rows for completed/dead tasks accumulate forever. Analogous to the existing `fs_events_retention_sweep` (server, runs every 60s, deletes rows older than `events_retention_days`). Wire the same shape for outbox:

- Cutoff: completed status + `updated_at < now - N days` (start with same 1-day default as fs_events; revisit if it harms debugging)
- Reuse the same scheduled task harness in `veda-server` startup
- Add a Prometheus counter mirroring `veda_fs_events_retention_swept_total`
- Probably ~30 lines of code

Search anchors: grep for `fs_events retention` in `crates/veda-server/src/` to find the existing pattern.

### 2. Three sidecar cross-review follow-ups (from alpha-plan §6)

Tracked in `docs/plans/alpha-plan-2026-04-29.md` "Sidecar / capabilities 跟进":

- **SSE actively clears sidecar miss cache** — when the summary worker finishes `/docs`, server emits `summary_ready` SSE event and FUSE's `invalidate_caches` clears the matching `sidecar_miss` entry. Today users wait up to `attr_ttl=5s` for new sidecars to appear; alpha+ ergonomics fix.
- **`/v1/capabilities` auth** — route is currently unauthenticated. Under hardened deployments (reverse proxy enforcing auth on `/v1/*`), probe fails-closed instead of fail-open. Fix by exposing it under a `/healthz`-style path, or wiring the bearer through the probe call.
- **`FsService` reserved-name integration test** — `crates/veda-core/tests/fs_service_test.rs` doesn't sweep every mutating call site (write_file / append / mkdir / copy / rename) against the reserved-name guard. Helper is unit-tested + call sites are mechanical (low risk), but a per-entry sweep would lock the contract.

## Explicitly NOT doing for alpha

Joe's call as of 2026-05-15:

- **Week 6 dogfood** — alpha-plan §451 reserved a week for personal dogfood as the "ship to tryouts?" gate. Joe is collapsing that into "ship now and watch"; the FUSE mount is already running writeback-mode against the prod server, which acts as ad-hoc dogfood.
- **C5 historical timezone data migration** — `time_zone='+00:00'` is pinned on new connections (commit `a74ed6c`), pre-existing rows weren't migrated. Document as alpha known-limitation: redeploy fresh, no in-place migration. Add a one-line note to `docs/alpha-tryout.md` known-limitations section.
- **CI publish veda-server binary** — currently CI only ships `veda` (CLI) + `veda-fuse`, so server upgrades require building from source on the box (`cd /data/rust/veda && cargo build -p veda-server`). Backlogged for the next release, not blocking alpha.
- **Everything in alpha-plan §1 "明确不做"** — multi-replica, doc-level ACL/quota, login rate-limit (C6), FUSE truncate of >50 MB files (C7/C10), tree-sitter chunking, PDF/OCR, OpenAPI/SDK/Connector, etc. These are deferred-by-design with explicit "promote to beta if X" triggers in the plan.

## References (don't re-read everything)

- `docs/plans/alpha-plan-2026-04-29.md` — full plan + Known Limitations table; §6 has the now-mostly-empty post-alpha route list
- `docs/plans/fuse-writeback-plan.md` — completed (every milestone shipped); can mark closed in `alpha-plan §6` next time the file is touched
- `ARCHITECTURE.md` — `veda-fuse` bullet was updated to describe the writeback subsystem
- `skill.md` — `--write-mode` / `--write-debounce-ms` documented
- `~/.claude/projects/-Users-konglingqiao-code-personal-veda/memory/MEMORY.md` — long-running project memory; relevant files are `reference_prod_box.md`, `project_review_findings_status.md` (now mostly historical — review findings C-series all done or deferred)

## Suggested next-session moves

1. **Start with S8** (smallest, most contained). Skill: just direct implementation, no special agent needed. ~30 lines + 1 integration test against the existing fs_events fixture.
2. **Then sidecar follow-ups** — `gh:review-pr` style approach won't help; these are direct edits in `veda-server` (`/v1/capabilities` auth) and `veda-fuse/src/sse.rs` (cache invalidation handler). The reserved-name integration test goes in `crates/veda-core/tests/`.
3. **Mark fuse-writeback DONE** in `docs/plans/alpha-plan-2026-04-29.md` §6 — change the entry from "to do" to "delivered (commits d15d3f6 → 13edb01)".
4. **Update `docs/alpha-tryout.md`** known-limitations with the C5 fresh-redeploy note.

After those four are landed, alpha is shippable to tryout users. The Week 6 dogfood gate is replaced by "Joe runs writeback-mode mount + uses Veda daily; surface issues as they appear."

## Open external state (alpha-soft can ignore but worth knowing)

- `todos.md` does **not** exist at repo root (was created earlier in this session and intentionally deleted; items either resolved or moved to alpha-plan backlog).
- Last GitLab CI run was for tag `0.0.12-test` (publish succeeded; verified via API).
- Both remotes are in sync at `13edb01`.
