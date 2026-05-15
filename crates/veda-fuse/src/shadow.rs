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
///
/// macFUSE/libfuse3 acceptance of inodes >= 1<<63 isn't formally
/// verified yet — the FUSE protocol treats `ino` as opaque u64 except
/// for 0 (`FUSE_UNKNOWN_INO`) and 1 (root), so this should work, but
/// Day 3 (the first commit that actually returns these to the kernel
/// via `lookup`/`getattr`) is the live test. If the kernel rejects
/// them, fall back to a non-conflicting low range (e.g. start above
/// `InodeTable::next_ino`'s ceiling and have the two allocators
/// coordinate) — see docs/plans/fuse-writeback-plan.md.
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
    /// Parent inode the entry was created/adopted under. Needed by
    /// `mark_committed` to evict the entry's basename from
    /// `pending_children` once a LocalOnly upload lands on the server.
    /// rename() (Day 4) updates this field when reparenting.
    pub parent_ino: u64,
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
        // Reclaim the prior entry's bytes if we're replacing one
        // (e.g. unlink-then-create within a session leaves no entry,
        // but a defensive remove keeps total_bytes correct against
        // any future caller that calls create_local twice).
        if let Some(prior) = self.entries.remove(path) {
            self.total_bytes = self.total_bytes.saturating_sub(prior.data.len());
        }
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
                parent_ino,
            },
        );
        (local_ino, seq)
    }

    /// Materialise an existing server-side file as a `Clean` entry —
    /// called the first time FUSE opens a non-new file for writing.
    /// Caller supplies the current server revision and the existing
    /// bytes. Enforces caps just like `write_at`; if a cap would be
    /// breached the store is left untouched and `InsertError` is
    /// returned (caller should fall through to a sync flush of any
    /// prior entry, then write directly against the server).
    pub fn adopt_server_file(
        &mut self,
        path: &str,
        parent_ino: u64,
        local_ino: u64,
        base_rev: i32,
        initial_bytes: Vec<u8>,
    ) -> Result<u64, InsertError> {
        let new_len = initial_bytes.len();
        if new_len > self.max_per_file {
            return Err(InsertError::PerFileCapExceeded);
        }
        // Account for a possible replacement of an existing entry:
        // count only the delta against the prior data length, so
        // re-adopt of the same path doesn't double-count.
        let prior_len = self.entries.get(path).map(|e| e.data.len()).unwrap_or(0);
        let after_total = self
            .total_bytes
            .saturating_sub(prior_len)
            .saturating_add(new_len);
        if after_total > self.max_total {
            return Err(InsertError::TotalCapExceeded);
        }
        self.tombstones.remove(path);
        self.total_bytes = after_total;
        self.seq += 1;
        self.entries.insert(
            path.to_string(),
            ShadowEntry {
                data: initial_bytes,
                base_rev: Some(base_rev),
                kind: EntryKind::Clean,
                mtime: SystemTime::now(),
                seq: self.seq,
                local_ino,
                parent_ino,
            },
        );
        Ok(self.seq)
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
    /// `(prior_kind, new_seq)`: the kind the entry held before tombstone
    /// (None if no entry existed) and the seq value the global counter
    /// reached after the bump — used by the commit queue to send a
    /// precise `Cancel { path, seq }` token to its worker.
    pub fn tombstone(&mut self, path: &str, parent_ino: u64) -> (Option<EntryKind>, u64) {
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
        (prior, self.seq)
    }

    /// Clear a tombstone — used when re-creating a path that was
    /// recently deleted. `create_local`/`adopt_server_file` already
    /// do this implicitly; standalone callers shouldn't need it.
    pub fn clear_tombstone(&mut self, path: &str) -> bool {
        self.tombstones.remove(path)
    }

    /// Move a shadow entry from `old_path` to `new_path`, updating
    /// pending_children for both parent inodes. Bumps seq so any
    /// in-flight commit captured under the old path becomes stale.
    /// Returns the bumped seq so the caller can issue
    /// `Cancel{old_path, old_seq}` + `Touch{new_path, new_seq}`.
    /// No-op if `old_path` doesn't exist; clears any tombstone at
    /// `new_path` to make the rename idempotent against a recent
    /// delete-then-rename pattern (`git mv` over an old file).
    pub fn rename(
        &mut self,
        old_path: &str,
        new_path: &str,
        new_parent_ino: u64,
    ) -> Option<u64> {
        let mut entry = self.entries.remove(old_path)?;
        // Remove from old parent's pending set.
        if let Some(old_name) = path_basename(old_path) {
            if let Some(set) = self.pending_children.get_mut(&entry.parent_ino) {
                set.remove(old_name);
                if set.is_empty() {
                    self.pending_children.remove(&entry.parent_ino);
                }
            }
        }
        self.tombstones.remove(new_path);
        if let Some(new_name) = path_basename(new_path) {
            self.pending_children
                .entry(new_parent_ino)
                .or_default()
                .insert(new_name.to_string());
        }
        entry.parent_ino = new_parent_ino;
        self.seq += 1;
        entry.seq = self.seq;
        entry.mtime = SystemTime::now();
        self.entries.insert(new_path.to_string(), entry);
        Some(self.seq)
    }

    /// Mark an entry's commit as successful at the given server
    /// revision. Silently no-op when entry was canceled / superseded
    /// (seq mismatch); the worker recognises that and falls back to
    /// the stale-seq cleanup path.
    ///
    /// The entry stays in `pending_children` after promotion. That
    /// keeps the shadow as the up-to-date authority for `readdir` /
    /// `lookup` until the dir cache naturally refreshes — otherwise
    /// a file would briefly disappear from `ls` between
    /// `mark_committed` (drops it from the overlay) and the next
    /// server listing TTL expiry (server side now has it). Readdir
    /// dedups against the server listing so Clean entries don't
    /// surface twice. tombstone() is what evicts pending_children;
    /// commit alone is not.
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
    fn tombstone_drops_entry_returns_kind_and_seq() {
        let mut s = ShadowStore::new();
        s.create_local("/foo.md", 1);
        s.write_at("/foo.md", 0, b"data").unwrap();
        let pre_seq = s.get("/foo.md").unwrap().seq;
        let (kind, ts_seq) = s.tombstone("/foo.md", 1);
        assert_eq!(kind, Some(EntryKind::LocalOnly));
        assert!(ts_seq > pre_seq);
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
        s.adopt_server_file("/foo.md", 1, 100, 1, b"old".to_vec()).unwrap();
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
        s.adopt_server_file("/foo.md", 1, 100, 1, b"v1".to_vec()).unwrap();
        let snapshot = s.write_at("/foo.md", 0, b"v2").unwrap();
        // Concurrent write while a commit is in flight bumps seq again.
        s.write_at("/foo.md", 0, b"v3").unwrap();
        s.mark_committed("/foo.md", snapshot, 2);
        // Still Dirty — the v2 commit is stale; v3 has yet to commit.
        assert_eq!(s.get("/foo.md").unwrap().kind, EntryKind::Dirty);
        assert_eq!(s.get("/foo.md").unwrap().base_rev, Some(1));
    }

    #[test]
    fn mark_committed_keeps_pending_child_for_overlay_continuity() {
        // Day 3 review fix: keeping the basename in pending_children
        // after promotion lets readdir/lookup serve the file from
        // shadow until the server-listing dir cache refreshes.
        // Without this, `ls` would briefly miss the file between
        // commit and the next dir cache TTL. readdir dedups Clean
        // entries against the server listing so they don't double-
        // surface. tombstone() is what evicts the overlay; commit
        // does not.
        let mut s = ShadowStore::new();
        let parent = 1;
        let (_, seq0) = s.create_local("/foo.md", parent);
        let seq1 = s.write_at("/foo.md", 0, b"hi").unwrap();
        assert!(s.pending_children(parent).unwrap().contains("foo.md"));
        s.mark_committed("/foo.md", seq1, 5);
        assert_eq!(s.get("/foo.md").unwrap().kind, EntryKind::Clean);
        assert!(
            s.pending_children(parent).unwrap().contains("foo.md"),
            "Clean entry stays in pending_children until tombstone"
        );
        assert!(seq1 > seq0);
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
    fn adopt_server_file_rejects_initial_buffer_over_per_file_cap() {
        let mut s = ShadowStore::with_caps(1024, 8);
        let err = s
            .adopt_server_file("/big", 1, 100, 1, vec![0u8; 9])
            .unwrap_err();
        assert_eq!(err, InsertError::PerFileCapExceeded);
        assert!(s.get("/big").is_none());
        assert_eq!(s.total_bytes(), 0);
    }

    #[test]
    fn adopt_server_file_re_adopt_does_not_double_count_total_bytes() {
        let mut s = ShadowStore::with_caps(20, 16);
        s.adopt_server_file("/a", 1, 100, 1, vec![0u8; 10]).unwrap();
        assert_eq!(s.total_bytes(), 10);
        // Re-adopt same path — total should reflect the new buffer,
        // not the sum of old and new.
        s.adopt_server_file("/a", 1, 100, 2, vec![0u8; 8]).unwrap();
        assert_eq!(s.total_bytes(), 8);
    }

    #[test]
    fn adopt_server_file_rejects_when_total_cap_breached() {
        let mut s = ShadowStore::with_caps(15, 16);
        s.adopt_server_file("/a", 1, 100, 1, vec![0u8; 10]).unwrap();
        // Adding another 10-byte adopt would push total to 20 > 15.
        let err = s
            .adopt_server_file("/b", 1, 101, 1, vec![0u8; 10])
            .unwrap_err();
        assert_eq!(err, InsertError::TotalCapExceeded);
        assert!(s.get("/b").is_none());
        assert_eq!(s.total_bytes(), 10);
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
    fn rename_moves_entry_and_pending_children() {
        let mut s = ShadowStore::new();
        let (_, seq0) = s.create_local("/a/x.md", 5);
        assert!(s.pending_children(5).unwrap().contains("x.md"));
        let new_seq = s.rename("/a/x.md", "/b/y.md", 6).expect("entry existed");
        assert!(new_seq > seq0);
        assert!(s.get("/a/x.md").is_none(), "old path gone");
        let entry = s.get("/b/y.md").expect("new path present");
        assert_eq!(entry.kind, EntryKind::LocalOnly);
        assert_eq!(entry.parent_ino, 6);
        assert!(s.pending_children(5).is_none(), "old parent's set drained");
        assert!(s.pending_children(6).unwrap().contains("y.md"));
    }

    #[test]
    fn rename_clears_tombstone_at_destination() {
        let mut s = ShadowStore::new();
        s.create_local("/dest", 1);
        s.tombstone("/dest", 1);
        assert!(s.is_tombstoned("/dest"));
        s.create_local("/src", 1);
        s.rename("/src", "/dest", 1).unwrap();
        assert!(!s.is_tombstoned("/dest"));
        assert!(s.get("/dest").is_some());
    }

    #[test]
    fn rename_nonexistent_path_returns_none() {
        let mut s = ShadowStore::new();
        assert!(s.rename("/nope", "/wherever", 1).is_none());
    }

    #[test]
    fn path_basename_handles_root_and_nested() {
        assert_eq!(path_basename("/foo"), Some("foo"));
        assert_eq!(path_basename("/a/b/c.md"), Some("c.md"));
        assert_eq!(path_basename("/"), None);
        assert_eq!(path_basename(""), None);
    }
}
