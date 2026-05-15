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
use std::collections::BinaryHeap;
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
    /// (Re)schedule a commit for `path`. If a previous Touch is still
    /// pending in the heap, the worker will see this newer seq later
    /// and the old heap entry self-skips via `is_current` check.
    Touch { path: String, seq: u64 },
    /// Bookkeeping signal. The store itself is the source of truth via
    /// its global seq counter; the worker doesn't need to do anything
    /// special on Cancel beyond recognising that a future heap pop
    /// will fail `is_current`. We keep the variant for symmetry with
    /// the plan's protocol and because it's free.
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
    // (deadline, path, captured_seq). Reverse → min-heap by deadline.
    let mut heap: BinaryHeap<Reverse<(Instant, String, u64)>> = BinaryHeap::new();

    loop {
        let now = Instant::now();
        let next_wait = heap
            .peek()
            .map(|Reverse((d, _, _))| d.saturating_duration_since(now))
            .unwrap_or(Duration::from_secs(3600));

        match rx.recv_timeout(next_wait) {
            Ok(FlusherCmd::Touch { path, seq }) => {
                heap.push(Reverse((Instant::now() + window, path, seq)));
            }
            Ok(FlusherCmd::Cancel { .. }) => {
                // Implicit via seq mismatch on fire; the explicit
                // signal is informational. No-op intentionally.
            }
            Ok(FlusherCmd::Drain { ack }) => {
                while let Some(Reverse((_, path, seq))) = heap.pop() {
                    fire(&client, &shadow, &path, seq);
                }
                let _ = ack.send(());
            }
            Ok(FlusherCmd::Shutdown) => break,
            Err(RecvTimeoutError::Timeout) => {
                let now = Instant::now();
                while let Some(Reverse((d, _, _))) = heap.peek() {
                    if *d > now {
                        break;
                    }
                    let Reverse((_, path, seq)) = heap.pop().unwrap();
                    fire(&client, &shadow, &path, seq);
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn fire(
    client: &Arc<dyn CommitClient>,
    shadow: &Arc<Mutex<ShadowStore>>,
    path: &str,
    captured_seq: u64,
) {
    // Snapshot under lock; release the lock for the blocking HTTP so
    // FUSE read/write/lookup handlers in the main thread don't stall.
    let snap = {
        let store = shadow.lock().unwrap();
        if !store.is_current(path, captured_seq) {
            return; // superseded or canceled while waiting
        }
        store.snapshot(path)
    };
    let Some((data, snap_seq, base_rev)) = snap else {
        return;
    };
    let result = client.write_file(path, &data, base_rev);
    let store = shadow.lock().unwrap();
    if !store.is_current(path, snap_seq) {
        // Canceled/superseded while the HTTP was in flight. If the
        // server accepted the write, we now have bytes on the server
        // that should be gone; do a best-effort delete to converge.
        drop(store);
        if result.is_ok() {
            let _ = client.delete(path);
        }
        return;
    }
    drop(store);
    match result {
        Ok(Some(new_rev)) => {
            shadow.lock().unwrap().mark_committed(path, snap_seq, new_rev);
        }
        Ok(None) => {
            // Server accepted but didn't echo a revision (e.g. 304-style
            // content-unchanged path). Leave the entry Dirty so the
            // next touch re-attempts; harmless.
        }
        Err(e) => {
            warn!(path = %path, err = %e, "commit_queue PUT failed; leaving entry Dirty");
        }
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
