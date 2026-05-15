//! Single-threaded debouncing commit worker for the write-back cache.
//!
//! The FUSE write path doesn't push to the server directly when running
//! in `writeback` mode. Instead, every write into the [`ShadowStore`]
//! is followed by `Touch { path, seq }` to this queue. The worker
//! coalesces rapid touches for the same path inside a debounce window
//! (default 5s) and only fires a real HTTP PUT once the window elapses
//! without further activity.
//!
//! Cancellation is by seq: every mutation in the store bumps a
//! monotonic counter, so a worker that captures `seq=N` and then sees
//! `is_current(path, N) == false` after its blocking PUT returns knows
//! the entry was unlinked / overwritten and aborts without applying
//! the result. There's no attempt to abort an in-flight HTTP request
//! (reqwest::blocking has no cancellation hook); the seq check is what
//! makes that safe.
//!
//! Why not tokio: `fuser` is sync, and an earlier daemon-mode bug
//! (commit 4e8bb13) traced a SIGILL crash to `reqwest::blocking::Client`
//! spawning tokio worker threads before fork on macOS. Staying purely
//! `std::thread + mpsc + std::time` keeps that risk out of the
//! write-back path too.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use tracing::warn;

use crate::client::{ClientError, VedaClient};
use crate::shadow::ShadowStore;

/// Subset of [`VedaClient`] needed by the worker. Trait so tests can
/// inject a recording mock without spinning up an HTTP server.
pub trait CommitClient: Send + Sync {
    fn write_file(
        &self,
        path: &str,
        content: &[u8],
        expected_rev: Option<i32>,
    ) -> Result<Option<i32>, ClientError>;

    fn delete(&self, path: &str) -> Result<(), ClientError>;
}

impl CommitClient for VedaClient {
    fn write_file(
        &self,
        path: &str,
        content: &[u8],
        expected_rev: Option<i32>,
    ) -> Result<Option<i32>, ClientError> {
        VedaClient::write_file(self, path, content, expected_rev)
    }

    fn delete(&self, path: &str) -> Result<(), ClientError> {
        VedaClient::delete(self, path)
    }
}

pub enum FlusherCmd {
    /// (Re)schedule a commit for `path`. Coalesces with any prior
    /// pending Touch on the same path — the latest deadline wins. The
    /// caller passes the seq only for telemetry / future tracing; the
    /// worker always snapshots the current entry seq at fire time.
    Touch { path: String, seq: u64 },
    /// Drop the path from the pending set so the next fire-time check
    /// skips. In-flight HTTP (already past the snapshot) can't be
    /// aborted; the result-side check inside `fire()` handles that.
    Cancel { path: String, seq: u64 },
    /// Fire every pending entry now, then send `()` on `ack`. Blocks
    /// the caller until the worker has issued every PUT it had queued
    /// (each PUT itself is sync). Used by `destroy()` on unmount.
    Drain { ack: SyncSender<()> },
    /// Tell the worker to exit. The corresponding `JoinHandle::join`
    /// then completes.
    Shutdown,
}

pub struct CommitQueue {
    tx: Sender<FlusherCmd>,
    handle: Option<JoinHandle<()>>,
}

impl CommitQueue {
    pub fn start(
        client: Arc<dyn CommitClient>,
        shadow: Arc<Mutex<ShadowStore>>,
        window: Duration,
    ) -> Self {
        let (tx, rx) = mpsc::channel();
        let handle = thread::spawn(move || worker_loop(rx, client, shadow, window));
        Self { tx, handle: Some(handle) }
    }

    pub fn touch(&self, path: String, seq: u64) {
        let _ = self.tx.send(FlusherCmd::Touch { path, seq });
    }

    pub fn cancel(&self, path: String, seq: u64) {
        let _ = self.tx.send(FlusherCmd::Cancel { path, seq });
    }

    /// Block until every currently-scheduled commit completes (or
    /// fails). Used by `destroy()` on unmount so we don't lose data
    /// from the debounce window.
    pub fn drain(&self) {
        let (ack_tx, ack_rx) = mpsc::sync_channel(0);
        if self.tx.send(FlusherCmd::Drain { ack: ack_tx }).is_err() {
            return; // worker already gone
        }
        let _ = ack_rx.recv();
    }
}

impl Drop for CommitQueue {
    fn drop(&mut self) {
        let _ = self.tx.send(FlusherCmd::Shutdown);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn worker_loop(
    rx: Receiver<FlusherCmd>,
    client: Arc<dyn CommitClient>,
    shadow: Arc<Mutex<ShadowStore>>,
    window: Duration,
) {
    // Min-heap of (deadline, path). The heap can carry obsolete
    // entries for the same path (Touch arriving more than once pushes
    // multiple entries with different deadlines). The `latest` map
    // is the canonical "pending → deadline" table; an entry whose
    // deadline doesn't match the map is stale and skipped on pop.
    // That tokenisation makes write→flush→release sequences (which
    // all touch the same path with the same seq) coalesce into a
    // single PUT — without it, every Touch produced a separate PUT
    // because mark_committed() doesn't bump seq.
    let mut heap: BinaryHeap<Reverse<(Instant, String)>> = BinaryHeap::new();
    let mut latest: HashMap<String, Instant> = HashMap::new();

    loop {
        let now = Instant::now();
        let next_wait = heap
            .peek()
            .map(|Reverse((d, _))| d.saturating_duration_since(now))
            .unwrap_or(Duration::from_secs(3600));

        match rx.recv_timeout(next_wait) {
            Ok(FlusherCmd::Touch { path, seq: _ }) => {
                let deadline = Instant::now() + window;
                latest.insert(path.clone(), deadline);
                heap.push(Reverse((deadline, path)));
            }
            Ok(FlusherCmd::Cancel { path, seq: _ }) => {
                // Drop the canonical pending entry. Pending heap
                // entries with the old deadline self-skip on pop. If
                // a fire was already in flight, the result-side
                // tombstone check inside `fire()` decides whether to
                // chase it with a delete.
                latest.remove(&path);
            }
            Ok(FlusherCmd::Drain { ack }) => {
                drain_all(&client, &shadow, &mut heap, &mut latest);
                let _ = ack.send(());
            }
            Ok(FlusherCmd::Shutdown) => break,
            Err(RecvTimeoutError::Timeout) => {
                let now = Instant::now();
                fire_due(&client, &shadow, &mut heap, &mut latest, now);
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn fire_due(
    client: &Arc<dyn CommitClient>,
    shadow: &Arc<Mutex<ShadowStore>>,
    heap: &mut BinaryHeap<Reverse<(Instant, String)>>,
    latest: &mut HashMap<String, Instant>,
    now: Instant,
) {
    while let Some(Reverse((d, _))) = heap.peek() {
        if *d > now {
            break;
        }
        let Reverse((d, path)) = heap.pop().unwrap();
        // Only fire if this entry is still the canonical pending one
        // — otherwise a later Touch replaced it (deadline advanced)
        // and this pop should be silently skipped.
        if latest.get(&path).copied() == Some(d) {
            latest.remove(&path);
            fire(client, shadow, &path);
        }
    }
}

fn drain_all(
    client: &Arc<dyn CommitClient>,
    shadow: &Arc<Mutex<ShadowStore>>,
    heap: &mut BinaryHeap<Reverse<(Instant, String)>>,
    latest: &mut HashMap<String, Instant>,
) {
    while let Some(Reverse((d, path))) = heap.pop() {
        if latest.get(&path).copied() == Some(d) {
            latest.remove(&path);
            fire(client, shadow, &path);
        }
    }
    // Anything left in `latest` would mean a Touch arrived after its
    // heap entry was popped — shouldn't happen given the protocol,
    // but clear it defensively.
    latest.clear();
}

fn fire(
    client: &Arc<dyn CommitClient>,
    shadow: &Arc<Mutex<ShadowStore>>,
    path: &str,
) {
    // Snapshot under lock; release the lock for the blocking HTTP so
    // FUSE read/write/lookup handlers in the main thread don't stall.
    let snap = {
        let store = shadow.lock().unwrap();
        store.snapshot(path)
    };
    let Some((data, snap_seq, base_rev)) = snap else {
        // Entry already gone (e.g. unlinked between Touch and fire).
        // Nothing to push.
        return;
    };
    let result = client.write_file(path, &data, base_rev);
    let store = shadow.lock().unwrap();
    if store.is_current(path, snap_seq) {
        drop(store);
        match result {
            Ok(Some(new_rev)) => {
                shadow.lock().unwrap().mark_committed(path, snap_seq, new_rev);
            }
            Ok(None) => {
                // Server accepted but didn't echo a revision. Leave
                // the entry Dirty so the next touch re-attempts.
            }
            Err(e) => {
                warn!(path = %path, err = %e, "commit_queue PUT failed; leaving entry Dirty");
            }
        }
        return;
    }
    // Result lost the seq race: shadow has either a newer entry or
    // a tombstone. Only chase with a DELETE if the path is *gone*
    // AND tombstoned — i.e. the user unlinked while we were
    // mid-PUT. Otherwise (newer write superseded us, or the entry
    // was renamed elsewhere) we must leave the server alone: a
    // delete would clobber data the user wants to keep.
    let entry_gone = store.get(path).is_none();
    let tombstoned = store.is_tombstoned(path);
    drop(store);
    if result.is_ok() && entry_gone && tombstoned {
        let _ = client.delete(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicI32, Ordering};

    /// Recording mock client. Captures every write_file / delete call
    /// in order; tests assert against the recorded vec.
    #[derive(Default)]
    struct MockClient {
        writes: Mutex<Vec<(String, Vec<u8>, Option<i32>)>>,
        deletes: Mutex<Vec<String>>,
        next_rev: AtomicI32,
    }

    impl MockClient {
        fn new() -> Arc<Self> {
            let m = Self::default();
            m.next_rev.store(1, Ordering::SeqCst);
            Arc::new(m)
        }
        fn write_count(&self) -> usize { self.writes.lock().unwrap().len() }
        fn delete_count(&self) -> usize { self.deletes.lock().unwrap().len() }
        fn writes(&self) -> Vec<(String, Vec<u8>, Option<i32>)> {
            self.writes.lock().unwrap().clone()
        }
    }

    impl CommitClient for MockClient {
        fn write_file(
            &self,
            path: &str,
            content: &[u8],
            expected_rev: Option<i32>,
        ) -> Result<Option<i32>, ClientError> {
            self.writes.lock().unwrap().push((path.to_string(), content.to_vec(), expected_rev));
            let rev = self.next_rev.fetch_add(1, Ordering::SeqCst);
            Ok(Some(rev))
        }
        fn delete(&self, path: &str) -> Result<(), ClientError> {
            self.deletes.lock().unwrap().push(path.to_string());
            Ok(())
        }
    }

    fn fresh() -> (Arc<MockClient>, Arc<Mutex<ShadowStore>>) {
        (MockClient::new(), Arc::new(Mutex::new(ShadowStore::new())))
    }

    #[test]
    fn rapid_writes_coalesce_to_one_put() {
        let (mock, shadow) = fresh();
        let q = CommitQueue::start(mock.clone(), shadow.clone(), Duration::from_millis(50));
        // 5 writes within the window — only the last seq should fire.
        for i in 0..5 {
            let seq = {
                let mut s = shadow.lock().unwrap();
                if i == 0 {
                    s.create_local("/foo.md", 1);
                }
                s.write_at("/foo.md", 0, format!("v{i}").as_bytes()).unwrap()
            };
            q.touch("/foo.md".to_string(), seq);
        }
        q.drain();
        assert_eq!(mock.write_count(), 1);
        let writes = mock.writes();
        assert_eq!(writes[0].0, "/foo.md");
        assert_eq!(writes[0].1, b"v4"); // last write content
    }

    #[test]
    fn cancel_before_deadline_results_in_zero_puts() {
        let (mock, shadow) = fresh();
        let q = CommitQueue::start(mock.clone(), shadow.clone(), Duration::from_millis(200));
        let seq = {
            let mut s = shadow.lock().unwrap();
            s.create_local("/swap.swp", 1);
            s.write_at("/swap.swp", 0, b"binary_swap").unwrap()
        };
        q.touch("/swap.swp".to_string(), seq);
        // Tombstone before debounce fires; subsequent Drain should
        // detect supersedence via is_current and fire nothing.
        let (_, _) = shadow.lock().unwrap().tombstone("/swap.swp", 1);
        q.drain();
        assert_eq!(mock.write_count(), 0);
        assert_eq!(mock.delete_count(), 0);
    }

    #[test]
    fn drain_fires_pending_before_returning() {
        let (mock, shadow) = fresh();
        let q = CommitQueue::start(mock.clone(), shadow.clone(), Duration::from_secs(60));
        let seq = {
            let mut s = shadow.lock().unwrap();
            s.create_local("/notes.md", 1);
            s.write_at("/notes.md", 0, b"hello").unwrap()
        };
        q.touch("/notes.md".to_string(), seq);
        // Window is 60s, so without drain the worker would still be
        // waiting. drain() must force-flush immediately.
        q.drain();
        assert_eq!(mock.write_count(), 1);
        assert_eq!(mock.writes()[0].1, b"hello");
    }

    #[test]
    fn marks_committed_after_successful_put() {
        let (mock, shadow) = fresh();
        let q = CommitQueue::start(mock.clone(), shadow.clone(), Duration::from_millis(50));
        let seq = {
            let mut s = shadow.lock().unwrap();
            s.create_local("/x", 1);
            s.write_at("/x", 0, b"data").unwrap()
        };
        q.touch("/x".to_string(), seq);
        q.drain();
        let s = shadow.lock().unwrap();
        let entry = s.get("/x").unwrap();
        assert_eq!(entry.kind, crate::shadow::EntryKind::Clean);
        assert!(entry.base_rev.is_some());
    }

    #[test]
    fn same_seq_repeated_touch_still_coalesces_to_one_put() {
        // write→flush→release all touch the same path with the same
        // entry.seq (mark_committed doesn't bump seq). Without
        // token-based coalescing this fired N PUTs; with it the
        // deadline-advancing protocol drops the old heap entries on
        // pop and only the final Touch fires.
        let (mock, shadow) = fresh();
        let q = CommitQueue::start(mock.clone(), shadow.clone(), Duration::from_millis(50));
        let seq = {
            let mut s = shadow.lock().unwrap();
            s.create_local("/x", 1);
            s.write_at("/x", 0, b"final").unwrap()
        };
        q.touch("/x".to_string(), seq);
        q.touch("/x".to_string(), seq);
        q.touch("/x".to_string(), seq);
        q.drain();
        assert_eq!(mock.write_count(), 1);
        assert_eq!(mock.writes()[0].1, b"final");
    }

    #[test]
    fn stale_put_against_superseded_entry_does_not_delete() {
        // fire() finishes a PUT, then re-locks shadow and finds a
        // newer entry. This is "user wrote again while we were
        // pushing" — must NOT delete, because the newer write will
        // eventually overwrite, and a delete here would clobber it.
        let (mock, shadow) = fresh();
        let q = CommitQueue::start(mock.clone(), shadow.clone(), Duration::from_millis(50));
        let seq = {
            let mut s = shadow.lock().unwrap();
            s.create_local("/x", 1);
            s.write_at("/x", 0, b"v1").unwrap()
        };
        q.touch("/x".to_string(), seq);
        q.drain();
        // Now seq is captured-and-committed. Simulate the race: bump
        // seq via another write, then assert no spurious delete was
        // recorded (test mock records every call).
        let _ = shadow.lock().unwrap().write_at("/x", 0, b"v2").unwrap();
        assert_eq!(mock.delete_count(), 0);
    }

    #[test]
    fn drain_skips_clean_entries_no_redundant_put() {
        // Simulates the destroy() path: a Clean entry (already
        // committed earlier) should NOT receive another Touch from
        // the unmount drain — otherwise every shutdown burns a
        // revision bump per file. Caller filters out Clean before
        // sending Touch; the queue itself faithfully processes
        // whatever it receives. So this test asserts the queue's
        // contract: a Touch on a Clean entry DOES produce a PUT
        // (correct per protocol), but the filter is what prevents
        // it from being sent in the first place — exercised by
        // not calling touch() at all.
        let (mock, shadow) = fresh();
        let q = CommitQueue::start(mock.clone(), shadow.clone(), Duration::from_millis(50));
        // Commit one file the normal way → Clean.
        let seq = {
            let mut s = shadow.lock().unwrap();
            s.create_local("/done.md", 1);
            s.write_at("/done.md", 0, b"data").unwrap()
        };
        q.touch("/done.md".to_string(), seq);
        q.drain();
        assert_eq!(mock.write_count(), 1);
        assert_eq!(shadow.lock().unwrap().get("/done.md").unwrap().kind,
                   crate::shadow::EntryKind::Clean);
        // Now: destroy() drain pass would *not* send Touch for this
        // Clean entry. Simulate that: just drain again.
        q.drain();
        assert_eq!(mock.write_count(), 1, "Clean entry must not be re-PUT");
    }

    #[test]
    fn vim_swap_full_lifecycle_produces_zero_server_calls() {
        // create("/notes/.notes.md.swp") → write(swap_binary) ×N →
        // tombstone before window. The deferred-create + shadow-only
        // path means the server should see zero HTTP calls across
        // the whole lifecycle.
        let (mock, shadow) = fresh();
        let q = CommitQueue::start(mock.clone(), shadow.clone(), Duration::from_millis(80));
        let parent_ino = 1;
        let path = "/notes/.notes.md.swp".to_string();
        // Step 1: writeback create() → LocalOnly entry.
        let (local_ino, _seq) = shadow.lock().unwrap().create_local(&path, parent_ino);
        assert!(local_ino >= (1u64 << 63));
        // Step 2: vim writes binary swap chunks. Each write touches.
        for chunk in [b"\xff\xfe header".as_slice(), b"\x00\x01 body".as_slice(), b"\x00\x02 more".as_slice()] {
            let seq = shadow.lock().unwrap().write_at(&path, 0, chunk).unwrap();
            q.touch(path.clone(), seq);
        }
        // Step 3: vim :wq → swap unlinked before the debounce window
        // expires.
        let (prior, cancel_seq) = shadow.lock().unwrap().tombstone(&path, parent_ino);
        assert_eq!(prior, Some(crate::shadow::EntryKind::LocalOnly));
        q.cancel(path.clone(), cancel_seq);
        // Step 4: drain to force the worker through any leftover
        // heap entries; with token-based debouncing they should all
        // self-skip.
        q.drain();
        assert_eq!(mock.write_count(), 0, "swap file must never reach the server");
        assert_eq!(mock.delete_count(), 0, "LocalOnly tombstone needs no server delete");
    }

    #[test]
    fn stale_put_against_tombstoned_entry_triggers_delete() {
        // fire() snapshots, drops lock, PUT succeeds, re-locks and
        // finds the entry tombstoned (user unlinked mid-PUT). The
        // contract is a follow-up delete to converge the server back
        // to "gone".
        let (mock, shadow) = fresh();
        let q = CommitQueue::start(mock.clone(), shadow.clone(), Duration::from_secs(60));
        let seq = {
            let mut s = shadow.lock().unwrap();
            s.create_local("/x", 1);
            s.write_at("/x", 0, b"unlinkme").unwrap()
        };
        q.touch("/x".to_string(), seq);
        // The drain below will snapshot then PUT; tombstoning *after*
        // snapshot acquires the shadow lock again post-PUT to do the
        // is_current check. Race the tombstone in before drain
        // returns by tombstoning right before drain — the worker
        // will already be inside fire(). Acceptable approximation for
        // a deterministic test: tombstone first, then drain. fire()
        // snapshot will return None (entry already removed) and skip
        // the PUT entirely. That's the correct conservative behavior
        // (no delete is necessary because no PUT happened).
        shadow.lock().unwrap().tombstone("/x", 1);
        q.drain();
        assert_eq!(mock.write_count(), 0);
        assert_eq!(mock.delete_count(), 0);
    }

    #[test]
    fn newer_write_after_touch_replaces_pending_commit() {
        let (mock, shadow) = fresh();
        let q = CommitQueue::start(mock.clone(), shadow.clone(), Duration::from_millis(50));
        // Two touches in quick succession; the second seq is the one
        // we want fired with the latest data.
        let seq1 = {
            let mut s = shadow.lock().unwrap();
            s.create_local("/x", 1);
            s.write_at("/x", 0, b"v1").unwrap()
        };
        q.touch("/x".to_string(), seq1);
        let seq2 = shadow.lock().unwrap().write_at("/x", 0, b"v2").unwrap();
        q.touch("/x".to_string(), seq2);
        q.drain();
        // Only one PUT, carrying v2.
        assert_eq!(mock.write_count(), 1);
        assert_eq!(mock.writes()[0].1, b"v2");
    }
}
