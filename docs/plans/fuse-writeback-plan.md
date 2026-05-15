# FUSE write-back + debounce + cancel-on-unlink

**Status**: design locked 2026-05-15, not yet started
**Owner**: Joe
**Estimated effort**: 5 days
**Depends on**: nothing (server-side unchanged)

## Problem

`veda-fuse` current write path is sync write-through: every `flush`/`fsync`/`release`
calls `client.write_file()` synchronously. This breaks four real workflows:

1. **vim**: creates `.<name>.swp` (binary), server rejects with HTTP 400 "must be
   valid UTF-8", FUSE returns `-EIO`, vim shows `E72: Close error on swap file`.
   Reproducible on `/Users/konglingqiao/veda-mnt`; daemon log confirmed.
2. **git**: `.git/index.lock` → rename → `.git/index` lands the lockfile on
   server before the rename collapses it.
3. **IDEs**: JetBrains `workspace.xml`, VS Code `.vscode/settings.json` auto-save
   every keystroke; current path turns each keystroke into an HTTP PUT.
4. **`create()` premature commit**: `create()` does `client.write_file(path, b"",
   None)` immediately, so even before the user's first byte the server has the
   empty file. Every vim-swap-open pollutes the workspace.

The naive "let server accept binary" fix would let all four workflows above
upload their churn to the server, polluting the vector index and burning
bandwidth. The right fix is a **client-side write-back cache with debounced
commits and cancel-on-unlink**.

Reference: drive9 (Go) solves this with ShadowStore + flushDebouncer +
CommitQueue.CancelPath in `pkg/fuse/`. See
<https://github.com/mem9-ai/drive9>.

## Design

Three new modules under `crates/veda-fuse/src/`:

- `shadow.rs` — in-memory write buffer + tombstones, keyed by remote path
- `commit_queue.rs` — dedicated worker thread, debounce + cancel
- (modified) `fs.rs` — FUSE handlers route through shadow + queue

No tokio (fuser is sync; reqwest blocking; we already learned fork+tokio is
unsafe on macOS, see commit 4e8bb13).

### ShadowStore (`shadow.rs`)

```rust
pub struct ShadowStore {
    entries: HashMap<String, ShadowEntry>,  // key = remote canonical path
    tombstones: HashSet<String>,            // unlinked since last server sync
    seq: u64,                               // monotonic, used for stale detection
    total_bytes: usize,
    max_total: usize,                       // default 50 MB — OOM ceiling
    max_per_file: usize,                    // default 10 MB — per-entry cap
    pending_children: HashMap<u64, HashSet<String>>,  // parent_ino → names
}

struct ShadowEntry {
    data: Vec<u8>,
    base_rev: Option<i32>,    // None for LocalOnly; server revision for Dirty/Clean
    kind: EntryKind,
    mtime: SystemTime,        // synthetic; used for getattr on shadow entries
    seq: u64,                 // bumped on every write/cancel/unlink
    local_ino: u64,           // stable for the entry's lifetime; survives upgrade
}

enum EntryKind {
    LocalOnly,    // server has never seen this path; create() target
    Dirty,        // server has it, local is newer (pending commit)
    Clean,        // local == server (GC candidate after grace period)
}
```

Why `String` not `PathBuf`: the remote canonical path is a normalized string
("/notes/foo.md"); `PathBuf` conflates with OS-path concepts (case sensitivity,
backslash handling on Windows) and breaks `Hash` consistency across platforms.

`local_ino` allocated from a reserved high range (e.g. `1u64 << 63 + counter`)
to never collide with server-issued inodes. Verify fuser/macFUSE accepts
high-bit inodes during Day 1; fall back to a non-conflicting low range if not.

Size caps degrade gracefully: write into an entry that would exceed
`max_per_file` triggers immediate sync flush (drop back to current behavior for
that file). Total cap reached → reject the write with `-ENOSPC`. Defensive only
— alpha workspace files are <1 MB.

### CommitQueue (`commit_queue.rs`)

One dedicated OS thread, owns a min-heap by deadline + a channel for commands.

```rust
pub enum FlusherCmd {
    Touch { path: String, seq: u64 },    // (re)schedule for now + window
    Cancel { path: String, seq: u64 },   // drop pending; called on unlink/rename
    Drain { ack: SyncSender<()> },       // unmount: block caller until empty
    Shutdown,
}

pub struct CommitQueue {
    tx: Sender<FlusherCmd>,
    handle: JoinHandle<()>,
}

impl CommitQueue {
    pub fn start(
        client: VedaClient,
        shadow: Arc<Mutex<ShadowStore>>,
        window: Duration,            // default 5s
    ) -> Self { ... }
}
```

Worker loop sketch:

```text
loop {
    timeout = match heap.peek() {
        Some(entry) => entry.deadline.saturating_duration_since(Instant::now()),
        None => Duration::from_secs(3600),
    };
    match rx.recv_timeout(timeout) {
        Ok(Touch { path, seq })  => heap.push((now + window, path, seq)),
        Ok(Cancel { path, seq }) => mark seq stale in shadow; heap entry skipped on fire,
        Ok(Drain { ack })        => fire all due immediately, ack.send(()),
        Ok(Shutdown)             => break,
        Err(Timeout)             => fire all heap entries past now,
    }
}
```

Firing a path:

```text
1. snapshot = shadow.lock().get(path).map(|e| (e.data.clone(), e.seq, e.base_rev))
   — drop the lock before the blocking HTTP
2. result = client.write_file(&path, &data, base_rev)
3. shadow.lock().mark_committed(&path, snapshot.seq, result)
   — bumps seq is no-op if entry.seq != snapshot.seq (was canceled/superseded)
   — on success: kind = Clean, base_rev = new_rev
   — on stale + result.is_ok(): the server now has data that should be gone;
     enqueue a best-effort DELETE
```

Cloning `data` under the lock is wasteful for big files but trivially safe.
Day-5 optimization: snapshot via `mem::replace` if entry is Clean-on-Dirty
transition, or wrap data in `Arc<Vec<u8>>` to share without copying.

### FUSE handler changes (`fs.rs`)

| Handler | New behavior |
|---|---|
| `create` | `shadow.insert_local_only(path)` → allocate `local_ino` → return `ReplyCreate`. **No server call.** |
| `write` | append/overwrite into `entry.data` at offset; bump `entry.seq`; `tx.send(Touch{path, seq})` |
| `flush`, `fsync`, `release` | `tx.send(Touch{path, seq})` then return `reply.ok()` — no sync wait |
| `lookup`, `getattr` | check shadow.entries first (returns synthetic attr from `data.len()` + `mtime`); check tombstones (return ENOENT); else server |
| `readdir` | server entries ∪ `pending_children[parent_ino]` − tombstones |
| `unlink` | `shadow.tombstone(path)` + bump seq; `tx.send(Cancel{path, seq})`; if entry was `Dirty`/`Clean`, send `client.delete(path)` synchronously |
| `rename` | `tx.send(Cancel{old})`; rename entry in shadow under new path with bumped seq; `tx.send(Touch{new})` |
| `destroy` (unmount) | `(ack_tx, ack_rx) = sync_channel(0); tx.send(Drain{ack: ack_tx}); ack_rx.recv()` — block unmount until queue is empty |

### Configuration

CLI flag on `veda-fuse mount`:

- `--write-mode {sync,writeback}` — default `sync` until dogfood passes; flip to
  `writeback` after a week of clean operation
- `--write-debounce-ms <N>` — only relevant in writeback mode; default `5000`

Env var equivalents `VEDA_FUSE_WRITE_MODE`, `VEDA_FUSE_WRITE_DEBOUNCE_MS`.

`sync` mode goes through current code path verbatim (regression safety net).

### Key sequences (worked examples)

**vim swap file** (the trigger case):

```
1. vim opens notes.md
2. vim create(/notes/.notes.md.swp)
   → shadow: LocalOnly entry, local_ino=L1
   → server: 0 calls
3. vim write(L1, swap_binary) repeatedly
   → shadow: data grows, seq bumps; Touch{path, seq} reschedules each time
4. user :wq
5. vim unlink(/notes/.notes.md.swp)
   → shadow.tombstone, Cancel{path, seq}, server: 0 calls
6. vim flush(notes.md) — UTF-8 text
   → shadow: Dirty entry, Touch scheduled
   → 5s later: worker fires PUT /notes/notes.md, marks Clean
```

Total server calls for the entire vim session: **1 PUT** (the actual file).
Currently: 1 PUT (succeeds, the main file) + N failed PUT attempts for the
swap (each one a -EIO back to vim, producing E72).

**git clone into mount**:

```
1. git clone creates .git/index.lock, writes pack data
   → shadow: LocalOnly
2. git rename .git/index.lock → .git/index (within ms)
   → shadow: Cancel old path, Touch new path
3. 5s later: worker PUTs .git/index only
```

Server never sees `.git/index.lock`.

## Test plan

### Unit (`shadow.rs`, `commit_queue.rs`)

- ShadowStore: insert/get/tombstone/seq-bump; size cap rejection; readdir overlay
- CommitQueue: schedule/reschedule/cancel; drain semantics; stale seq drops result

### Integration with mock VedaClient

Mock counts HTTP calls. Spin up real `VedaFs` + mock client + simulated FUSE
ops. Assertions:

| Scenario | Expected HTTP calls |
|---|---|
| `create(.swp)` + 10 × `write` + `unlink(.swp)` inside window | 0 PUT, 0 DELETE |
| `create(real.md)` + `write` + `flush` + wait window | 1 PUT |
| 5 × `flush` within window | 1 PUT (coalesced) |
| `rename` during pending flush | 1 PUT @ new path, 0 @ old |
| `unmount` with pending entries | drained PUTs complete before unmount returns |
| stale `write_file` succeeds after `unlink` | 1 follow-up DELETE |
| `create` + immediate `unlink` (LocalOnly tombstone) | 0 PUT, 0 DELETE |

### Manual

- `vim notes.md` end-to-end on `/Users/konglingqiao/veda-mnt`
- `git clone https://github.com/jooekong/veda.git ~/veda-mnt/scratch/veda-clone`
- JetBrains create `.idea/workspace.xml`
- `dd if=/dev/zero of=~/veda-mnt/big.bin bs=1M count=100` → expect size cap
  degrade to sync flush, no OOM

## Milestones

Each day's work must leave the tree in a shippable state behind
`--write-mode=sync` (default).

| Day | Scope | Verification |
|---|---|---|
| 1 | `shadow.rs` skeleton: ShadowStore, LocalOnly insert, pending_children, size cap, local_ino allocator. Verify fuser accepts reserved-range inodes. | Unit tests pass; `sync` mode unchanged. |
| 2 | `commit_queue.rs`: worker thread, schedule/cancel/drain. Wire `flush`/`fsync`/`release` to `Touch` under `writeback` mode only. | Mock integration: rapid `write+flush` coalesces to 1 PUT. |
| 3 | `create()` defers; `lookup`/`getattr`/`readdir` overlay shadow. Tombstone handling in `unlink`. | vim swap end-to-end: 0 server calls for `.swp`. |
| 4 | `rename` cancel + reschedule. Stale-seq DELETE follow-up. Size cap fallback. Mock integration suite complete. | All mock assertions pass. |
| 5 | `destroy` drain, real-server smoke on `/Users/konglingqiao/veda-mnt` (vim, git, dd). Update ARCHITECTURE.md FUSE section. | Joe runs `writeback` mode daily for a week. |

After +1 week dogfood: flip `--write-mode` default to `writeback`, keep `sync`
as escape hatch.

## Tradeoffs & decisions

- **In-memory only, no tmpdir**: crash loses up-to-`window` of pending writes.
  Acceptable for single-user alpha; revisit only if a user actually reports
  pain.
- **No PathPolicy::Ephemeral / no filename blacklist**: covered by tombstone +
  unlink-within-window. Avoids the perennial "add `.swo` / `.swx` / `4913` /
  next-IDE's-pattern" maintenance burden.
- **5s window (not 2s)**: amortizes IDE auto-save bursts and git's
  multi-write-then-rename pattern. User-visible save-to-server-visible latency
  is 5s, acceptable for a knowledge base.
- **No HTTP cancellation**: blocking reqwest can't be aborted cleanly. Seq
  counter + follow-up DELETE gives eventual consistency without unsafe thread
  kills.
- **Size caps as safety net, not policy**: defensive against `dd`/`cat
  /dev/urandom`; not a deliberate "files over 10 MB should go elsewhere"
  policy.
- **`local_ino` reserved range**: high-bit `1<<63 + counter` if fuser/macFUSE
  accept; otherwise an explicitly-tracked allocator below normal server inode
  range. Decided on Day 1.

## Out of scope

- Persistent shadow (tmpdir) — defer to a "FUSE durability" follow-up
- Server-side binary upload — separate feature, not gated by this work
- Multi-writer conflict resolution beyond current `If-Match` — alpha is
  single-user
- Read-side caching changes — `ReadCache` stays as-is
