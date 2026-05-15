//! In-memory write-back buffer for the FUSE filesystem.
//!
//! Holds dirty data between FUSE writes and the eventual server commit,
//! plus the negative state (tombstones, pending child names) needed by
//! `lookup`/`readdir` to present a coherent view of the workspace
//! before commits land.
//!
//! No persistence — survives only until the mount process exits. A
//! crash during the debounce window loses pending writes; acceptable
//! for single-user alpha. See docs/plans/fuse-writeback-plan.md.

use std::collections::{HashMap, HashSet};
use std::time::SystemTime;

/// Start of the local-only inode range. Server-issued inodes count up
/// from 2 (`InodeTable::next_ino`); reserving the high half avoids
/// collisions without coordinating between the two allocators.
pub const LOCAL_INO_BASE: u64 = 1u64 << 63;

/// Total in-memory cap across all entries. Soft policy: writes that
/// would breach the limit get rejected with ENOSPC; the caller can
/// degrade to a sync flush instead. Defensive against `dd
/// if=/dev/urandom`, not a feature gate.
pub const DEFAULT_MAX_TOTAL_BYTES: usize = 50 * 1024 * 1024;
pub const DEFAULT_MAX_PER_FILE_BYTES: usize = 10 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// Server has never seen this path. Created by FUSE `create()`.
    LocalOnly,
    /// Server has it, local has newer bytes pending a commit.
    Dirty,
    /// Local matches server. GC candidate after a grace period.
    Clean,
}

#[derive(Debug)]
pub struct ShadowEntry {
    pub data: Vec<u8>,
    pub base_rev: Option<i32>,
    pub kind: EntryKind,
    pub mtime: SystemTime,
    pub seq: u64,
    pub local_ino: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum InsertError {
    /// Per-file cap exceeded. Caller should fall back to a sync flush
    /// of the prior content (if any), then perform the write directly
    /// against the server.
    PerFileCapExceeded,
    /// Total store cap exceeded — typically a runaway producer. The
    /// FUSE handler should reject with ENOSPC.
    TotalCapExceeded,
}

pub struct ShadowStore {
    entries: HashMap<String, ShadowEntry>,
    tombstones: HashSet<String>,
    /// parent_ino → child names that exist locally but not on the
    /// server yet. Lets `readdir(parent)` merge them with the server
    /// listing without scanning all entries.
    pending_children: HashMap<u64, HashSet<String>>,
    seq: u64,
    next_local_ino: u64,
    total_bytes: usize,
    max_total: usize,
    max_per_file: usize,
}

impl ShadowStore {
    pub fn new() -> Self {
        Self::with_caps(DEFAULT_MAX_TOTAL_BYTES, DEFAULT_MAX_PER_FILE_BYTES)
    }

    pub fn with_caps(max_total: usize, max_per_file: usize) -> Self {
        Self {
            entries: HashMap::new(),
            tombstones: HashSet::new(),
            pending_children: HashMap::new(),
            seq: 0,
            next_local_ino: LOCAL_INO_BASE,
            total_bytes: 0,
            max_total,
            max_per_file,
        }
    }

    /// Allocate the next local-only inode. Inodes are never reused
    /// within a mount lifetime.
    pub fn alloc_local_ino(&mut self) -> u64 {
        let ino = self.next_local_ino;
        self.next_local_ino += 1;
        ino
    }

    /// Create a `LocalOnly` entry. Bumps seq, removes any tombstone
    /// for the path, and registers the name under its parent's
    /// pending_children. Returns (local_ino, seq).
    pub fn create_local(&mut self, path: &str, parent_ino: u64) -> (u64, u64) {
        self.tombstones.remove(path);
        let local_ino = self.alloc_local_ino();
        self.seq += 1;
        let seq = self.seq;
        if let Some(name) = path_basename(path) {
            self.pending_children
                .entry(parent_ino)
                .or_default()
                .insert(name.to_string());
        }
        self.entries.insert(
            path.to_string(),
            ShadowEntry {
                data: Vec::new(),
                base_rev: None,
                kind: EntryKind::LocalOnly,
                mtime: SystemTime::now(),
                seq,
                local_ino,
            },
        );
        (local_ino, seq)
    }

    /// Materialise an existing server-side file as a `Dirty` entry —
    /// called the first time FUSE opens a non-new file for writing.
    /// Caller supplies the current server revision and (optionally)
    /// the existing bytes. Returns the seq.
    pub fn adopt_server_file(
        &mut self,
        path: &str,
        local_ino: u64,
        base_rev: i32,
        initial_bytes: Vec<u8>,
    ) -> u64 {
        self.tombstones.remove(path);
        self.seq += 1;
        let len = initial_bytes.len();
        self.total_bytes = self.total_bytes.saturating_add(len);
        self.entries.insert(
            path.to_string(),
            ShadowEntry {
                data: initial_bytes,
                base_rev: Some(base_rev),
                kind: EntryKind::Clean,
                mtime: SystemTime::now(),
                seq: self.seq,
                local_ino,
            },
        );
        self.seq
    }

    /// Apply a write at `offset` to the entry's data buffer, growing
    /// as needed. Bumps seq. Returns the new seq, or `InsertError` if
    /// a cap would be breached (the entry is left untouched in that
    /// case).
    pub fn write_at(
        &mut self,
        path: &str,
        offset: u64,
        bytes: &[u8],
    ) -> Result<u64, InsertError> {
        let entry = self
            .entries
            .get_mut(path)
            .expect("write_at requires create_local or adopt_server_file first");
        let new_end = (offset as usize).saturating_add(bytes.len());
        let old_len = entry.data.len();
        let new_per_file = new_end.max(old_len);
        if new_per_file > self.max_per_file {
            return Err(InsertError::PerFileCapExceeded);
        }
        let growth = new_per_file.saturating_sub(old_len);
        if self.total_bytes.saturating_add(growth) > self.max_total {
            return Err(InsertError::TotalCapExceeded);
        }
        if entry.data.len() < new_end {
            entry.data.resize(new_end, 0);
        }
        entry.data[offset as usize..new_end].copy_from_slice(bytes);
        self.total_bytes += growth;
        entry.mtime = SystemTime::now();
        self.seq += 1;
        entry.seq = self.seq;
        if entry.kind == EntryKind::Clean {
            entry.kind = EntryKind::Dirty;
        }
        Ok(self.seq)
    }

    /// Drop the local entry, remember the path as tombstoned, remove
    /// its name from the parent's pending_children. Bumps seq so any
    /// in-flight commit for this path becomes stale on return. Returns
    /// the prior EntryKind, if any.
    pub fn tombstone(&mut self, path: &str, parent_ino: u64) -> Option<EntryKind> {
        let prior = self.entries.remove(path).map(|e| {
            self.total_bytes = self.total_bytes.saturating_sub(e.data.len());
            e.kind
        });
        self.tombstones.insert(path.to_string());
        if let Some(name) = path_basename(path) {
            if let Some(set) = self.pending_children.get_mut(&parent_ino) {
                set.remove(name);
                if set.is_empty() {
                    self.pending_children.remove(&parent_ino);
                }
            }
        }
        self.seq += 1;
        prior
    }

    /// Clear a tombstone — used when re-creating a path that was
    /// recently deleted. `create_local`/`adopt_server_file` already
    /// do this implicitly; standalone callers shouldn't need it.
    pub fn clear_tombstone(&mut self, path: &str) -> bool {
        self.tombstones.remove(path)
    }

    /// Mark an entry's commit as successful at the given server
    /// revision. Silently no-op when entry was canceled / superseded
    /// (seq mismatch); the worker recognises that and falls back to
    /// the stale-seq cleanup path.
    pub fn mark_committed(&mut self, path: &str, snapshot_seq: u64, new_rev: i32) {
        if let Some(entry) = self.entries.get_mut(path) {
            if entry.seq == snapshot_seq {
                entry.kind = EntryKind::Clean;
                entry.base_rev = Some(new_rev);
            }
        }
    }

    /// True if the entry exists and its seq matches the snapshot —
    /// i.e. no write/cancel happened since the snapshot. Worker
    /// threads call this after a long HTTP to decide whether to
    /// apply the result.
    pub fn is_current(&self, path: &str, snapshot_seq: u64) -> bool {
        self.entries
            .get(path)
            .map(|e| e.seq == snapshot_seq)
            .unwrap_or(false)
    }

    pub fn is_tombstoned(&self, path: &str) -> bool {
        self.tombstones.contains(path)
    }

    pub fn get(&self, path: &str) -> Option<&ShadowEntry> {
        self.entries.get(path)
    }

    pub fn pending_children(&self, parent_ino: u64) -> Option<&HashSet<String>> {
        self.pending_children.get(&parent_ino)
    }

    pub fn total_bytes(&self) -> usize {
        self.total_bytes
    }

    pub fn max_per_file(&self) -> usize {
        self.max_per_file
    }

    pub fn max_total(&self) -> usize {
        self.max_total
    }

    /// Snapshot the data + seq + base_rev under a single borrow. The
    /// commit-queue worker captures this, drops the lock, then makes
    /// the blocking HTTP call.
    pub fn snapshot(&self, path: &str) -> Option<(Vec<u8>, u64, Option<i32>)> {
        self.entries
            .get(path)
            .map(|e| (e.data.clone(), e.seq, e.base_rev))
    }
}

impl Default for ShadowStore {
    fn default() -> Self {
        Self::new()
    }
}

fn path_basename(path: &str) -> Option<&str> {
    path.rsplit('/').next().filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alloc_local_ino_is_monotonic_and_in_reserved_range() {
        let mut s = ShadowStore::new();
        let a = s.alloc_local_ino();
        let b = s.alloc_local_ino();
        assert_eq!(a, LOCAL_INO_BASE);
        assert_eq!(b, LOCAL_INO_BASE + 1);
        assert!(a >= LOCAL_INO_BASE);
    }

    #[test]
    fn create_local_inserts_entry_and_pending_child() {
        let mut s = ShadowStore::new();
        let parent = 1; // ROOT_INO
        let (ino, seq) = s.create_local("/foo.md", parent);
        assert_eq!(ino, LOCAL_INO_BASE);
        assert_eq!(seq, 1);
        let entry = s.get("/foo.md").unwrap();
        assert_eq!(entry.kind, EntryKind::LocalOnly);
        assert_eq!(entry.data.len(), 0);
        let names = s.pending_children(parent).unwrap();
        assert!(names.contains("foo.md"));
    }

    #[test]
    fn write_at_grows_buffer_and_bumps_seq() {
        let mut s = ShadowStore::new();
        s.create_local("/foo.md", 1);
        let seq1 = s.write_at("/foo.md", 0, b"hello").unwrap();
        assert_eq!(s.get("/foo.md").unwrap().data, b"hello");
        let seq2 = s.write_at("/foo.md", 5, b" world").unwrap();
        assert_eq!(s.get("/foo.md").unwrap().data, b"hello world");
        assert!(seq2 > seq1);
        assert_eq!(s.total_bytes(), 11);
    }

    #[test]
    fn write_at_with_gap_zero_fills() {
        let mut s = ShadowStore::new();
        s.create_local("/x", 1);
        s.write_at("/x", 10, b"end").unwrap();
        let buf = &s.get("/x").unwrap().data;
        assert_eq!(buf.len(), 13);
        assert_eq!(&buf[0..10], &[0u8; 10]);
        assert_eq!(&buf[10..], b"end");
    }

    #[test]
    fn per_file_cap_rejects_without_partial_write() {
        let mut s = ShadowStore::with_caps(1024, 8);
        s.create_local("/big", 1);
        s.write_at("/big", 0, b"01234567").unwrap();
        let err = s.write_at("/big", 8, b"X").unwrap_err();
        assert_eq!(err, InsertError::PerFileCapExceeded);
        // Buffer unchanged from successful write.
        assert_eq!(s.get("/big").unwrap().data, b"01234567");
        assert_eq!(s.total_bytes(), 8);
    }

    #[test]
    fn total_cap_rejects_when_other_entries_consume_budget() {
        let mut s = ShadowStore::with_caps(10, 8);
        s.create_local("/a", 1);
        s.write_at("/a", 0, b"01234").unwrap();
        s.create_local("/b", 1);
        s.write_at("/b", 0, b"56789").unwrap();
        assert_eq!(s.total_bytes(), 10);
        // Next byte tips us over total_cap=10.
        let err = s.write_at("/b", 5, b"X").unwrap_err();
        assert_eq!(err, InsertError::TotalCapExceeded);
    }

    #[test]
    fn tombstone_drops_entry_and_bumps_seq() {
        let mut s = ShadowStore::new();
        s.create_local("/foo.md", 1);
        s.write_at("/foo.md", 0, b"data").unwrap();
        let pre_seq = s.get("/foo.md").unwrap().seq;
        let kind = s.tombstone("/foo.md", 1).unwrap();
        assert_eq!(kind, EntryKind::LocalOnly);
        assert!(s.is_tombstoned("/foo.md"));
        assert!(s.get("/foo.md").is_none());
        assert!(s.pending_children(1).is_none()); // last child removed
        assert_eq!(s.total_bytes(), 0);
        // Any subsequent operation will see a seq > pre_seq, marking
        // an in-flight commit captured at pre_seq as stale.
        s.create_local("/bar.md", 1);
        assert!(s.get("/bar.md").unwrap().seq > pre_seq);
    }

    #[test]
    fn create_local_after_tombstone_clears_it() {
        let mut s = ShadowStore::new();
        s.create_local("/foo.md", 1);
        s.tombstone("/foo.md", 1);
        assert!(s.is_tombstoned("/foo.md"));
        s.create_local("/foo.md", 1);
        assert!(!s.is_tombstoned("/foo.md"));
        assert!(s.get("/foo.md").is_some());
    }

    #[test]
    fn mark_committed_promotes_dirty_to_clean_on_matching_seq() {
        let mut s = ShadowStore::new();
        s.adopt_server_file("/foo.md", 100, 1, b"old".to_vec());
        let seq_after_write = s.write_at("/foo.md", 0, b"new").unwrap();
        assert_eq!(s.get("/foo.md").unwrap().kind, EntryKind::Dirty);
        s.mark_committed("/foo.md", seq_after_write, 2);
        let entry = s.get("/foo.md").unwrap();
        assert_eq!(entry.kind, EntryKind::Clean);
        assert_eq!(entry.base_rev, Some(2));
    }

    #[test]
    fn mark_committed_ignores_stale_seq() {
        let mut s = ShadowStore::new();
        s.adopt_server_file("/foo.md", 100, 1, b"v1".to_vec());
        let snapshot = s.write_at("/foo.md", 0, b"v2").unwrap();
        // Concurrent write while a commit is in flight bumps seq again.
        s.write_at("/foo.md", 0, b"v3").unwrap();
        s.mark_committed("/foo.md", snapshot, 2);
        // Still Dirty — the v2 commit is stale; v3 has yet to commit.
        assert_eq!(s.get("/foo.md").unwrap().kind, EntryKind::Dirty);
        assert_eq!(s.get("/foo.md").unwrap().base_rev, Some(1));
    }

    #[test]
    fn is_current_detects_supersedence() {
        let mut s = ShadowStore::new();
        s.create_local("/x", 1);
        let snap = s.write_at("/x", 0, b"a").unwrap();
        assert!(s.is_current("/x", snap));
        s.write_at("/x", 0, b"b").unwrap();
        assert!(!s.is_current("/x", snap));
    }

    #[test]
    fn is_current_after_tombstone_is_false() {
        let mut s = ShadowStore::new();
        s.create_local("/x", 1);
        let snap = s.write_at("/x", 0, b"a").unwrap();
        s.tombstone("/x", 1);
        assert!(!s.is_current("/x", snap));
    }

    #[test]
    fn snapshot_returns_cloned_buffer() {
        let mut s = ShadowStore::new();
        s.create_local("/x", 1);
        s.write_at("/x", 0, b"hi").unwrap();
        let (buf, seq, base_rev) = s.snapshot("/x").unwrap();
        assert_eq!(buf, b"hi");
        assert!(seq > 0);
        assert_eq!(base_rev, None);
    }

    #[test]
    fn pending_children_tracks_multiple_names_per_parent() {
        let mut s = ShadowStore::new();
        s.create_local("/a/x.md", 5);
        s.create_local("/a/y.md", 5);
        let names = s.pending_children(5).unwrap();
        assert!(names.contains("x.md"));
        assert!(names.contains("y.md"));
        assert_eq!(names.len(), 2);
    }

    #[test]
    fn path_basename_handles_root_and_nested() {
        assert_eq!(path_basename("/foo"), Some("foo"));
        assert_eq!(path_basename("/a/b/c.md"), Some("c.md"));
        assert_eq!(path_basename("/"), None);
        assert_eq!(path_basename(""), None);
    }
}
