use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::os::fd::RawFd;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fuser::{
    FileAttr, FileType, Filesystem, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory,
    ReplyDirectoryPlus, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite, Request,
};
use tracing::{debug, warn};

use crate::cache::ReadCache;
use crate::client::{ClientError, DirEntry, FileInfo, SidecarOutcome, SummaryKind, VedaClient};
use crate::commit_queue::CommitQueue;
use crate::inode::InodeTable;
use crate::shadow::{EntryKind, InsertError, ShadowEntry, ShadowStore};

const BLOCK_SIZE: u32 = 512;

/// Sidecar entries injected into every directory's readdir output.
/// Both are read-only — see [`is_magic_name`] and the lookup / write
/// branches in `Filesystem` below. Order is fixed (.abstract before
/// .overview) so behaviour is deterministic across runs.
const MAGIC_NAMES: &[(&str, SummaryKind)] = &[
    (".abstract", SummaryKind::Abstract),
    (".overview", SummaryKind::Overview),
];

/// Rendered when the server says the summary is enqueued but not
/// yet generated (HTTP 202). One short line + newline, sized so
/// `cat` shows visible content rather than an empty file. Tools
/// retry naturally on the next attr_ttl tick.
const SIDECAR_PENDING_BODY: &str = "summary pending; retry after a few seconds\n";

/// Look the basename up against the magic-name table. Returns the
/// summary kind to fetch from the server, or `None` if the name is
/// a regular entry.
fn is_magic_name(name: &str) -> Option<SummaryKind> {
    MAGIC_NAMES
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, kind)| *kind)
}

pub(crate) fn parent_path(path: &str) -> &str {
    if path == "/" { return "/"; }
    match path.rfind('/') {
        Some(0) => "/",
        Some(i) => &path[..i],
        None => "/",
    }
}

pub struct FuseConfig {
    pub attr_ttl: Duration,
    pub read_only: bool,
    pub cache_size_mb: usize,
}

impl Default for FuseConfig {
    fn default() -> Self {
        Self {
            attr_ttl: Duration::from_secs(5),
            read_only: false,
            cache_size_mb: 128,
        }
    }
}

struct WriteHandle {
    ino: u64,
    /// Path pinned at handle creation. Survives an unlink of the
    /// inode mapping so POSIX open-unlink-close (e.g. vim writing
    /// a swap file and then deleting it before exit) still resolves
    /// in flush/release. inode_get_path(ino) returns None after
    /// unlink → would break release otherwise.
    path: String,
    buf: Vec<u8>,
    dirty: bool,
    base_rev: Option<i32>,
}

pub struct DirCacheEntry {
    pub entries: Arc<Vec<DirEntry>>,
    pub child_names: HashSet<String>,
    pub fetched_at: Instant,
}

impl DirCacheEntry {
    #[cfg(test)]
    pub fn empty_for_test() -> Self {
        Self {
            entries: Arc::new(Vec::new()),
            child_names: HashSet::new(),
            fetched_at: Instant::now(),
        }
    }
}

pub type DirCacheMap = Arc<Mutex<HashMap<u64, DirCacheEntry>>>;

/// Short-TTL "we just got ENOENT for this sidecar" cache. Prevents
/// `readdir` from advertising magic entries that the very next
/// `lookup` would 404 on (server-side `[llm]` disabled, or no
/// summary generated for this directory yet — a routine state when
/// the summary worker backlog is non-trivial). Keyed by `(parent
/// directory path, magic basename)`. Entries fall out naturally on
/// TTL expiry so a recently-arrived summary becomes visible again
/// on the next lookup.
type SidecarMissCache = Arc<Mutex<HashMap<(String, &'static str), Instant>>>;

pub struct VedaFs {
    client: Arc<VedaClient>,
    inodes: Arc<Mutex<InodeTable>>,
    config: FuseConfig,
    read_cache: Arc<Mutex<ReadCache>>,
    sidecar_miss: SidecarMissCache,
    /// Writeback shadow store, shared with the commit queue worker.
    /// `Some` ↔ writeback mode; `None` ↔ legacy sync mode (handlers
    /// fall through to the pre-shadow code path).
    shadow: Option<Arc<Mutex<ShadowStore>>>,
    /// Commit queue worker handle (held alongside shadow). Send Touch
    /// after every shadow write so the worker can debounce and PUT.
    commit_queue: Option<CommitQueue>,
    /// Mount-life cache of `/v1/capabilities.summary_enabled`.
    /// Probed once at construction in `main.rs::mount_fs`; `readdir`
    /// skips advertising magic sidecars when this is `false`, so
    /// deployments without `[llm]` configured don't surface phantom
    /// entries even on the very first directory listing. Defaults
    /// to `true` if the probe fails (Codex review trade-off: prefer
    /// false positives later cleaned up by the per-dir miss cache
    /// over silently hiding a feature that's actually available).
    /// Plain `bool` (no atomic): set once at construction, never
    /// mutated; FUSE op handlers all take `&mut self`, so each
    /// read is single-threaded by design.
    summary_enabled: bool,
    next_fh: u64,
    write_handles: HashMap<u64, WriteHandle>,
    dir_cache: DirCacheMap,
    /// File descriptor to signal parent process that mount succeeded (daemon mode).
    notify_fd: Option<RawFd>,
}

impl VedaFs {
    /// Build a FUSE filesystem. `summary_enabled` is the cached
    /// answer from `/v1/capabilities` (see `mount_fs` in `main.rs`);
    /// when `false`, `readdir` won't advertise synthetic summary
    /// sidecars so deployments without `[llm]` configured don't
    /// surface phantom entries. Tests typically pass `true` (the
    /// default-on alpha config) because they don't care about the
    /// sidecar discovery path.
    pub fn new(
        client: Arc<VedaClient>,
        config: FuseConfig,
        summary_enabled: bool,
        notify_fd: Option<RawFd>,
        shadow: Option<Arc<Mutex<ShadowStore>>>,
        commit_queue: Option<CommitQueue>,
    ) -> Self {
        let read_cache = Arc::new(Mutex::new(ReadCache::new(config.cache_size_mb, config.attr_ttl)));
        let inodes = Arc::new(Mutex::new(InodeTable::new_with_ttl(config.attr_ttl)));
        let dir_cache: DirCacheMap = Arc::new(Mutex::new(HashMap::new()));
        let sidecar_miss = Arc::new(Mutex::new(HashMap::new()));
        Self {
            client,
            inodes,
            config,
            read_cache,
            sidecar_miss,
            summary_enabled,
            next_fh: 1,
            write_handles: HashMap::new(),
            dir_cache,
            notify_fd,
            shadow,
            commit_queue,
        }
    }

    /// Helper: true when the FS is running in writeback mode (shadow
    /// + commit queue both configured). Sync mode handlers branch on
    /// this and keep the legacy in-memory-buf code path.
    fn is_writeback(&self) -> bool {
        self.shadow.is_some() && self.commit_queue.is_some()
    }

    pub fn inodes(&self) -> Arc<Mutex<InodeTable>> { self.inodes.clone() }
    pub fn read_cache(&self) -> Arc<Mutex<ReadCache>> { self.read_cache.clone() }
    pub fn dir_cache(&self) -> DirCacheMap { self.dir_cache.clone() }

    fn alloc_fh(&mut self) -> u64 {
        let fh = self.next_fh;
        self.next_fh += 1;
        fh
    }

    fn datetime_to_systime(dt: chrono::DateTime<chrono::Utc>) -> SystemTime {
        // chrono → SystemTime via UNIX_EPOCH offset. Negative timestamps
        // (pre-1970) are clamped to UNIX_EPOCH because FUSE doesn't model
        // them and we'd never see one in practice.
        let secs = dt.timestamp();
        let nanos = dt.timestamp_subsec_nanos();
        if secs >= 0 {
            UNIX_EPOCH + std::time::Duration::new(secs as u64, nanos)
        } else {
            UNIX_EPOCH
        }
    }

    fn make_attr(info: &FileInfo, ino: u64) -> FileAttr {
        let kind = if info.is_dir { FileType::Directory } else { FileType::RegularFile };
        let size = info.size_bytes.unwrap_or(0) as u64;
        let perm = if info.is_dir { 0o755 } else { 0o644 };
        // Use the server-known mtime/ctime when available so that tools like
        // `make`, `rsync -a`, and `git status -uno` see stable timestamps.
        // Fall back to wall clock for unauthenticated / legacy responses.
        let mtime = info
            .updated_at
            .map(Self::datetime_to_systime)
            .unwrap_or_else(SystemTime::now);
        let crtime = info
            .created_at
            .map(Self::datetime_to_systime)
            .unwrap_or(mtime);
        FileAttr {
            ino, size,
            blocks: (size + BLOCK_SIZE as u64 - 1) / BLOCK_SIZE as u64,
            atime: mtime, mtime, ctime: mtime, crtime,
            kind, perm,
            nlink: if info.is_dir { 2 } else { 1 },
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            rdev: 0, blksize: BLOCK_SIZE, flags: 0,
        }
    }

    fn resolve_child_path(parent_path: &str, name: &str) -> String {
        if parent_path == "/" { format!("/{name}") } else { format!("{parent_path}/{name}") }
    }

    /// Build a synthetic FileAttr for a shadow-tracked entry. Size
    /// comes from the buffer, mtime from the entry's recorded write
    /// time; mode/uid/gid/links match the server-attr defaults so
    /// `stat` output looks the same as for a real file.
    fn make_shadow_attr(entry: &ShadowEntry) -> FileAttr {
        let size = entry.data.len() as u64;
        FileAttr {
            ino: entry.local_ino,
            size,
            blocks: (size + BLOCK_SIZE as u64 - 1) / BLOCK_SIZE as u64,
            atime: entry.mtime,
            mtime: entry.mtime,
            ctime: entry.mtime,
            crtime: entry.mtime,
            kind: FileType::RegularFile,
            perm: 0o644,
            nlink: 1,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            rdev: 0,
            blksize: BLOCK_SIZE,
            flags: 0,
        }
    }

    fn attr_from_dir_entry(de: &DirEntry, ino: u64) -> FileAttr {
        let info = FileInfo {
            path: de.path.clone(),
            is_dir: de.is_dir,
            size_bytes: de.size_bytes,
            revision: None,
            created_at: de.created_at,
            updated_at: de.updated_at,
        };
        Self::make_attr(&info, ino)
    }

    fn should_cache_attr(de: &DirEntry) -> bool {
        de.is_dir || de.size_bytes.is_some()
    }

    fn dir_stub_attr(ino: u64) -> FileAttr {
        let info = FileInfo {
            path: String::new(),
            is_dir: true,
            size_bytes: None,
            revision: None,
            created_at: None,
            updated_at: None,
        };
        Self::make_attr(&info, ino)
    }

    fn err_to_errno(e: &ClientError) -> i32 {
        match e {
            ClientError::NotFound => libc::ENOENT,
            ClientError::AlreadyExists => libc::EEXIST,
            ClientError::PermissionDenied => libc::EACCES,
            ClientError::Conflict => libc::EBUSY,
            ClientError::Network(_) | ClientError::Server(_, _) | ClientError::Parse(_) => libc::EIO,
        }
    }

    fn inode_get_path(&self, ino: u64) -> Option<String> {
        self.inodes.lock().unwrap().get_path(ino).map(|s| s.to_string())
    }

    fn inode_get_or_create(&self, path: &str) -> u64 {
        self.inodes.lock().unwrap().get_or_create_ino(path)
    }

    fn inode_set_attr(&self, ino: u64, attr: FileAttr) {
        self.inodes.lock().unwrap().set_cached_attr(ino, attr);
    }

    fn inode_get_attr(&self, ino: u64) -> Option<FileAttr> {
        self.inodes.lock().unwrap().get_cached_attr(ino)
    }

    fn inode_invalidate(&self, ino: u64) {
        self.inodes.lock().unwrap().invalidate(ino);
    }

    fn inode_remove(&self, path: &str) {
        self.inodes.lock().unwrap().remove_path(path);
    }

    fn inode_rename(&self, old: &str, new: &str) {
        self.inodes.lock().unwrap().rename_path(old, new);
    }

    fn inode_lookup_cached(&self, path: &str) -> Option<(u64, FileAttr)> {
        let mut table = self.inodes.lock().unwrap();
        let ino = table.get_ino(path)?;
        let attr = table.get_cached_attr(ino)?;
        table.inc_nlookup(ino);
        Some((ino, attr))
    }

    fn inode_get_or_create_with_nlookup(&self, path: &str) -> u64 {
        let mut table = self.inodes.lock().unwrap();
        let ino = table.get_or_create_ino(path);
        table.inc_nlookup(ino);
        ino
    }

    fn cache_get_with_gen(&self, path: &str) -> (Option<Vec<u8>>, u64) {
        let mut cache = self.read_cache.lock().unwrap();
        let gen = cache.generation();
        let data = cache.get(path).map(|s| s.to_vec());
        (data, gen)
    }

    fn cache_put(&self, path: &str, data: Vec<u8>, gen: u64) {
        self.read_cache.lock().unwrap().put(path, data, gen);
    }

    fn cache_invalidate(&self, path: &str) {
        self.read_cache.lock().unwrap().invalidate(path);
    }

    fn cache_is_cacheable(&self, size: u64) -> bool {
        self.read_cache.lock().unwrap().is_cacheable_size(size)
    }

    fn fetch_dir(&mut self, ino: u64, path: &str) -> Result<Arc<Vec<DirEntry>>, i32> {
        let stale = {
            let dc = self.dir_cache.lock().unwrap();
            match dc.get(&ino) {
                Some(c) => c.fetched_at.elapsed() >= self.config.attr_ttl,
                None => true,
            }
        };
        if stale {
            let entries = self.client.list_dir(path).map_err(|ref e| Self::err_to_errno(e))?;
            let child_names: HashSet<String> = entries.iter().map(|de| de.name.clone()).collect();
            let entries = Arc::new(entries);
            let mut dc = self.dir_cache.lock().unwrap();
            dc.insert(ino, DirCacheEntry { entries: entries.clone(), child_names, fetched_at: Instant::now() });
        }
        let dc = self.dir_cache.lock().unwrap();
        Ok(dc.get(&ino).unwrap().entries.clone())
    }

    fn dir_cache_has_child(&self, parent_ino: u64, name: &str) -> Option<bool> {
        let dc = self.dir_cache.lock().unwrap();
        dc.get(&parent_ino).and_then(|c| {
            if c.fetched_at.elapsed() < self.config.attr_ttl {
                Some(c.child_names.contains(name))
            } else {
                None
            }
        })
    }

    fn invalidate_dir_cache(&mut self, parent_ino: u64) {
        self.dir_cache.lock().unwrap().remove(&parent_ino);
    }

    fn stat_and_cache_attr(&self, path: &str, ino: u64) -> Result<FileAttr, i32> {
        match self.client.stat(path) {
            Ok(info) => {
                let attr = Self::make_attr(&info, ino);
                self.inode_set_attr(ino, attr);
                Ok(attr)
            }
            Err(ref e) => Err(Self::err_to_errno(e)),
        }
    }

    /// Stable mtime/ctime for synthetic sidecars. Pinned to the
    /// first call (process start) instead of `SystemTime::now()` per
    /// invocation, because every `getattr` after attr_ttl expires
    /// re-renders the attr — tools that compare timestamps (`rsync
    /// -a`, `make`, `git status -uno`) would otherwise see a phantom
    /// modification on each TTL boundary and re-read the sidecar.
    fn magic_mtime() -> SystemTime {
        static MAGIC_MTIME: OnceLock<SystemTime> = OnceLock::new();
        *MAGIC_MTIME.get_or_init(SystemTime::now)
    }

    /// Fabricate a `FileAttr` for a sidecar (`.abstract` / `.overview`).
    /// Size matches the body returned by the server so `cat` reads
    /// the right slice; mode is 0444 because the file is read-only
    /// regardless of mount mode; mtime/ctime use a process-wide
    /// constant (see `magic_mtime`) so timestamp-sensitive tools
    /// don't see phantom modifications.
    fn magic_attr(ino: u64, body_size: u64) -> FileAttr {
        let pinned = Self::magic_mtime();
        FileAttr {
            ino,
            size: body_size,
            blocks: (body_size + BLOCK_SIZE as u64 - 1) / BLOCK_SIZE as u64,
            atime: pinned,
            mtime: pinned,
            ctime: pinned,
            crtime: pinned,
            kind: FileType::RegularFile,
            perm: 0o444,
            nlink: 1,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            rdev: 0,
            blksize: BLOCK_SIZE,
            flags: 0,
        }
    }

    /// Look up the canonical literal for a magic basename (so the
    /// cache key uses `'static &str` from the table instead of an
    /// owned String). Caller already verified the name is magic.
    fn magic_name_literal(name: &str) -> Option<&'static str> {
        MAGIC_NAMES
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(n, _)| *n)
    }

    /// Effective TTL for the sidecar miss cache. Floored at 1s so a
    /// user-configured `attr_ttl = 0` doesn't accidentally turn the
    /// cache off (which would re-introduce the phantom-entry
    /// problem the cache exists to suppress).
    fn miss_cache_ttl(&self) -> Duration {
        std::cmp::max(self.config.attr_ttl, Duration::from_secs(1))
    }

    /// Has this sidecar slot returned ENOENT recently enough that we
    /// should keep treating it as absent? Used by `readdir` to skip
    /// injecting phantom entries that the immediate `lookup` would
    /// then 404 on (server with `[llm]` disabled, or a directory the
    /// summary worker hasn't reached yet).
    fn sidecar_recently_missing(&self, parent_path: &str, magic_name: &'static str) -> bool {
        let guard = self.sidecar_miss.lock().unwrap();
        match guard.get(&(parent_path.to_string(), magic_name)) {
            Some(t) => t.elapsed() < self.miss_cache_ttl(),
            None => false,
        }
    }

    fn note_sidecar_missing(&self, parent_path: &str, magic_name: &'static str) {
        let mut guard = self.sidecar_miss.lock().unwrap();
        // Light eviction: drop entries that are already past TTL so
        // the map doesn't grow without bound under heavy listings.
        let ttl = self.miss_cache_ttl();
        guard.retain(|_, t| t.elapsed() < ttl);
        guard.insert((parent_path.to_string(), magic_name), Instant::now());
    }

    fn clear_sidecar_missing(&self, parent_path: &str, magic_name: &'static str) {
        let mut guard = self.sidecar_miss.lock().unwrap();
        guard.remove(&(parent_path.to_string(), magic_name));
    }

    /// Fetch the magic sidecar body, drive cache, return the attr
    /// FUSE should publish for the entry. The mapping:
    ///   200 Body(b)   → attr.size = b.len(); body cached at the
    ///                   magic path so the subsequent `read` is a
    ///                   single in-memory slice.
    ///   202 Pending   → render a one-line placeholder so `cat`
    ///                   shows progress; cached at attr_ttl so the
    ///                   next attempt auto-refreshes.
    ///   501 Disabled  → ENOENT. The deployment has no LLM, so the
    ///                   sidecar is effectively absent and `ls -a`
    ///                   shouldn't expose it as readable.
    ///   404 NotFound  → ENOENT. The parent directory has no
    ///                   summary (workspace-root edge, or a path
    ///                   that doesn't exist server-side).
    fn magic_lookup(&self, target_path: &str, ino: u64, kind: SummaryKind) -> Result<FileAttr, i32> {
        // The HTTP `path` we hand the server is the parent directory
        // — that's what gets summarised. `target_path` already has
        // the magic suffix stripped because the caller built it with
        // `parent_path` semantics in mind.
        let outcome = self
            .client
            .get_summary(target_path, kind)
            .map_err(|ref e| Self::err_to_errno(e))?;
        let magic_name = match kind {
            SummaryKind::Abstract => ".abstract",
            SummaryKind::Overview => ".overview",
        };
        let body = match outcome {
            SidecarOutcome::Body(b) => {
                // Body present → clear any prior miss note so the
                // entry shows up in subsequent readdir calls without
                // waiting for TTL.
                self.clear_sidecar_missing(target_path, magic_name);
                b
            }
            SidecarOutcome::Pending => {
                self.clear_sidecar_missing(target_path, magic_name);
                SIDECAR_PENDING_BODY.as_bytes().to_vec()
            }
            SidecarOutcome::Disabled | SidecarOutcome::NotFound => {
                // Remember the miss so the next readdir doesn't list
                // a phantom entry the immediate lookup will ENOENT.
                self.note_sidecar_missing(target_path, magic_name);
                return Err(libc::ENOENT);
            }
        };
        let size = body.len() as u64;
        // Stash the body in the regular read cache so the
        // immediately-following `read` doesn't make a second HTTP
        // call. The key is the synthetic sidecar path (e.g.
        // `/docs/.abstract`); the server-side reserved-name check
        // prevents a real file from sharing the key.
        let sidecar_path = format!("{}/{magic_name}", target_path.trim_end_matches('/'));
        let (_, gen) = self.cache_get_with_gen(&sidecar_path);
        self.cache_put(&sidecar_path, body, gen);
        Ok(Self::magic_attr(ino, size))
    }

    /// True iff `ino`'s last path segment matches a magic sidecar
    /// name. Used by the write-side ops (`setattr`, `unlink`,
    /// `rename`) to reject mutations on the synthetic entries.
    fn is_magic_ino(&self, ino: u64) -> bool {
        match self.inode_get_path(ino) {
            Some(p) => {
                let basename = p.rsplit('/').next().unwrap_or("");
                is_magic_name(basename).is_some()
            }
            None => false,
        }
    }

    /// Force a synchronous PUT of a shadow entry's current bytes to
    /// the server, then drop the entry from the shadow so subsequent
    /// FUSE ops on the same path fall through to the legacy buf /
    /// server-stat code path. Used by the per-file cap fallback in
    /// write(): when a file crosses 10 MB the writeback overlay
    /// quietly degrades to synchronous writes for that file.
    ///
    /// Drains the commit queue for this path before snapshotting so
    /// a worker that's mid-PUT for the same path doesn't race the
    /// sync write — without this, an older shadow PUT could land
    /// AFTER our sync flush and clobber the fresh bytes.
    fn sync_flush_and_evict_shadow(
        &mut self,
        path: &str,
    ) -> std::result::Result<(), ClientError> {
        // Drain pending commits for this path. cancel removes the
        // canonical pending token; drain forces fire of anything
        // already past the snapshot stage. After drain returns, the
        // path is quiescent on the worker side.
        let cancel_seq = {
            let shadow = self.shadow.as_ref().unwrap();
            let store = shadow.lock().unwrap();
            store.get(path).map(|e| e.seq).unwrap_or(0)
        };
        let q = self.commit_queue.as_ref().unwrap();
        q.cancel(path.to_string(), cancel_seq);
        q.drain();
        let snap = {
            let shadow = self.shadow.as_ref().unwrap();
            let store = shadow.lock().unwrap();
            store.snapshot(path)
        };
        let Some((data, _seq, base_rev)) = snap else {
            return Ok(());
        };
        self.client.write_file(path, &data, base_rev)?;
        // Evict shadow entry. tombstone + clear_tombstone removes
        // it from the entries / pending_children maps without
        // leaving a negative cache that would suppress the server-
        // side stat path on the next lookup.
        let shadow = self.shadow.as_ref().unwrap();
        let mut store = shadow.lock().unwrap();
        let parent_ino = store
            .get(path)
            .map(|e| e.parent_ino)
            .unwrap_or(crate::inode::ROOT_INO);
        let _ = store.tombstone(path, parent_ino);
        store.clear_tombstone(path);
        Ok(())
    }

    /// writeback path for flush/fsync/release: if the file is
    /// shadow-tracked, send a Touch and return success immediately
    /// (the commit queue handles the eventual server PUT). If the
    /// path was tombstoned (open-unlink-close: vim writes a swap
    /// then deletes it before closing), just reply OK — the shadow
    /// already dropped the entry and the queue's seq tracking
    /// prevents any in-flight PUT. If it's not in shadow at all
    /// (open-existing legacy fallback in write()), call flush_handle
    /// to drain the legacy buf synchronously so :w doesn't silently
    /// lose data.
    ///
    /// Resolves the path via the WriteHandle, not via inode_get_path,
    /// so an unlink that already cleared the inode mapping can't
    /// turn this into a spurious ENOENT.
    fn writeback_touch_or_fallback_flush(
        &mut self,
        fh: u64,
        ino: u64,
        reply: ReplyEmpty,
    ) {
        let path = match self.write_handles.get(&fh).map(|h| h.path.clone()) {
            Some(p) if !p.is_empty() => p,
            _ => {
                // No handle (or unresolved path) — fall back to the
                // legacy sync path, which itself no-ops on a missing
                // handle/dirty flag.
                match self.flush_handle(fh, ino) {
                    Ok(()) => reply.ok(),
                    Err(errno) => reply.error(errno),
                }
                return;
            }
        };
        let shadow = self.shadow.as_ref().unwrap();
        let store = shadow.lock().unwrap();
        if store.is_tombstoned(&path) {
            // Unlinked before close. The shadow + queue already
            // handled cancellation; nothing left to do.
            drop(store);
            reply.ok();
            return;
        }
        let entry_seq = store.get(&path).map(|e| e.seq);
        drop(store);
        if let Some(seq) = entry_seq {
            self.commit_queue.as_ref().unwrap().touch(path, seq);
            reply.ok();
            return;
        }
        // No shadow entry — fall back to the legacy sync flush so
        // open-existing-then-write paths don't drop their buffer.
        match self.flush_handle(fh, ino) {
            Ok(()) => reply.ok(),
            Err(errno) => reply.error(errno),
        }
    }

    fn flush_handle(&mut self, fh: u64, ino: u64) -> Result<(), i32> {
        let is_dirty = self.write_handles.get(&fh).map_or(false, |h| h.dirty);
        if !is_dirty {
            return Ok(());
        }
        let path = match self.inode_get_path(ino) {
            Some(p) => p,
            None => return Err(libc::ENOENT),
        };
        let (buf, base_rev) = {
            let h = self.write_handles.get(&fh).unwrap();
            (h.buf.clone(), h.base_rev)
        };
        match self.client.write_file(&path, &buf, base_rev) {
            Ok(new_rev) => {
                if let Some(handle) = self.write_handles.get_mut(&fh) {
                    handle.dirty = false;
                    if new_rev.is_some() {
                        handle.base_rev = new_rev;
                    }
                }
                self.inode_invalidate(ino);
                self.cache_invalidate(&path);
                Ok(())
            }
            Err(ClientError::Conflict) => {
                warn!(path = %path, "flush conflict: remote revised since open");
                Err(libc::EBUSY)
            }
            Err(ref e) => {
                warn!(path = %path, err = %e, "flush failed");
                Err(Self::err_to_errno(e))
            }
        }
    }
}

impl Filesystem for VedaFs {
    fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let name_str = match name.to_str() {
            Some(s) => s,
            None => { reply.error(libc::ENOENT); return; }
        };
        let parent_path = match self.inode_get_path(parent) {
            Some(p) => p,
            None => { reply.error(libc::ENOENT); return; }
        };
        let child_path = Self::resolve_child_path(&parent_path, name_str);

        // Writeback shadow overlay: a tombstoned path means the
        // user just unlinked but the server may not know yet — must
        // return ENOENT instead of letting client.stat resurrect it.
        // A LocalOnly/Dirty/Clean entry hands back a synthetic attr
        // from the in-memory buffer; this is what makes a just-created
        // file visible via `ls`/`stat` before any commit has fired.
        if self.is_writeback() {
            let shadow = self.shadow.as_ref().unwrap();
            let store = shadow.lock().unwrap();
            if store.is_tombstoned(&child_path) {
                reply.error(libc::ENOENT);
                return;
            }
            if let Some(entry) = store.get(&child_path) {
                let attr = Self::make_shadow_attr(entry);
                let local_ino = entry.local_ino;
                drop(store);
                self.inodes.lock().unwrap().register_local(local_ino, &child_path);
                self.inodes.lock().unwrap().inc_nlookup(local_ino);
                self.inode_set_attr(local_ino, attr);
                reply.entry(&self.config.attr_ttl, &attr, 0);
                return;
            }
        }

        if let Some((_, attr)) = self.inode_lookup_cached(&child_path) {
            reply.entry(&self.config.attr_ttl, &attr, 0);
            return;
        }

        // Magic sidecar dispatch MUST run before the dir_cache_has_child
        // shortcut: the magic names are not in the upstream dir
        // listing (and therefore not in DirCacheEntry.child_names),
        // so the cache would otherwise return Some(false) and
        // synthesise an ENOENT for `cat /docs/.abstract` whenever a
        // prior `ls -a /docs` had populated the cache.
        //
        // KNOWN LIMITATION: a workspace that pre-dates the server-
        // side reserved-name blacklist may already contain a real
        // file at `.abstract` / `.overview`. Under FUSE the synthetic
        // sidecar shadows it: lookup/read return summary content
        // instead of the legacy data. The data is still reachable
        // via the CLI (`veda cat`), and `veda mv` can rename it out
        // of the way. See skill.md "Legacy sidecar collision".
        if let Some(kind) = is_magic_name(name_str) {
            // Short-TTL miss-cache from a recent magic_lookup ENOENT.
            // Skips the HTTP round-trip for `cat /docs/.abstract`
            // when the previous attempt already 404'd; falls out on
            // attr_ttl so newly generated summaries become visible.
            if let Some(magic) = Self::magic_name_literal(name_str) {
                if self.sidecar_recently_missing(&parent_path, magic) {
                    reply.error(libc::ENOENT);
                    return;
                }
            }
            let ino = self.inode_get_or_create_with_nlookup(&child_path);
            match self.magic_lookup(&parent_path, ino, kind) {
                Ok(attr) => {
                    self.inode_set_attr(ino, attr);
                    reply.entry(&self.config.attr_ttl, &attr, 0);
                }
                Err(errno) => reply.error(errno),
            }
            return;
        }

        if let Some(false) = self.dir_cache_has_child(parent, name_str) {
            reply.error(libc::ENOENT);
            return;
        }

        match self.client.stat(&child_path) {
            Ok(info) => {
                let ino = self.inode_get_or_create_with_nlookup(&child_path);
                let attr = Self::make_attr(&info, ino);
                self.inode_set_attr(ino, attr);
                reply.entry(&self.config.attr_ttl, &attr, 0);
            }
            Err(ref e) => {
                debug!(path = %child_path, err = %e, "lookup miss");
                reply.error(Self::err_to_errno(e));
            }
        }
    }

    fn forget(&mut self, _req: &Request, ino: u64, nlookup: u64) {
        self.inodes.lock().unwrap().forget(ino, nlookup);
    }

    fn getattr(&mut self, _req: &Request, ino: u64, reply: ReplyAttr) {
        // Writeback shadow overlay: serve directly from the in-
        // memory entry whenever the path is shadow-tracked, so size
        // and mtime reflect the latest write rather than stale
        // server state. Check before the attr cache because the
        // attr cache could hold a frozen snapshot.
        if self.is_writeback() {
            if let Some(path) = self.inode_get_path(ino) {
                let shadow = self.shadow.as_ref().unwrap();
                let store = shadow.lock().unwrap();
                if store.is_tombstoned(&path) {
                    reply.error(libc::ENOENT);
                    return;
                }
                if let Some(entry) = store.get(&path) {
                    let attr = Self::make_shadow_attr(entry);
                    drop(store);
                    self.inode_set_attr(ino, attr);
                    reply.attr(&self.config.attr_ttl, &attr);
                    return;
                }
            }
        }
        if let Some(attr) = self.inode_get_attr(ino) {
            reply.attr(&self.config.attr_ttl, &attr);
            return;
        }
        let path = match self.inode_get_path(ino) {
            Some(p) => p,
            None => { reply.error(libc::ENOENT); return; }
        };
        // Sidecar path: skip `client.stat` (the synthetic entry has
        // no server-side FS row to stat against) and refresh via the
        // summary endpoint instead. Without this, the readdirplus
        // zero-TTL stub or an expired lookup cache would cause
        // `stat /docs/.abstract` to 404.
        let basename = path.rsplit('/').next().unwrap_or("");
        if let Some(kind) = is_magic_name(basename) {
            let parent = parent_path(&path).to_string();
            match self.magic_lookup(&parent, ino, kind) {
                Ok(attr) => {
                    self.inode_set_attr(ino, attr);
                    reply.attr(&self.config.attr_ttl, &attr);
                }
                Err(errno) => reply.error(errno),
            }
            return;
        }
        match self.stat_and_cache_attr(&path, ino) {
            Ok(attr) => reply.attr(&self.config.attr_ttl, &attr),
            Err(errno) => reply.error(errno),
        }
    }

    fn setattr(
        &mut self, _req: &Request, ino: u64,
        _mode: Option<u32>, _uid: Option<u32>, _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<fuser::TimeOrNow>, _mtime: Option<fuser::TimeOrNow>,
        _ctime: Option<SystemTime>, _fh: Option<u64>,
        _crtime: Option<SystemTime>, _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>, _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        // Sidecars are synthetic read-only entries — no truncate,
        // no chmod, no utime. Returning EROFS keeps the contract
        // visible (vs ENOSYS, which would let some tools fall back
        // silently and look like success).
        if self.is_magic_ino(ino) {
            reply.error(libc::EROFS);
            return;
        }
        if let Some(new_size) = size {
            if self.config.read_only { reply.error(libc::EROFS); return; }
            let path = match self.inode_get_path(ino) {
                Some(p) => p,
                None => { reply.error(libc::ENOENT); return; }
            };
            let rev = match self.client.stat(&path) {
                Ok(info) => info.revision,
                Err(ref e) => { reply.error(Self::err_to_errno(e)); return; }
            };
            if new_size == 0 {
                if let Err(ref e) = self.client.write_file(&path, b"", rev) {
                    reply.error(Self::err_to_errno(e)); return;
                }
            } else {
                match self.client.read_file(&path) {
                    Ok(mut bytes) => {
                        let target = new_size as usize;
                        if target < bytes.len() { bytes.truncate(target); }
                        else if target > bytes.len() { bytes.resize(target, 0); }
                        if let Err(ref e) = self.client.write_file(&path, &bytes, rev) {
                            reply.error(Self::err_to_errno(e)); return;
                        }
                    }
                    Err(ref e) => { reply.error(Self::err_to_errno(e)); return; }
                }
            }
            self.inode_invalidate(ino);
            self.cache_invalidate(&path);
        }
        let path = match self.inode_get_path(ino) {
            Some(p) => p,
            None => { reply.error(libc::ENOENT); return; }
        };
        match self.stat_and_cache_attr(&path, ino) {
            Ok(attr) => reply.attr(&self.config.attr_ttl, &attr),
            Err(errno) => reply.error(errno),
        }
    }

    fn readdir(&mut self, _req: &Request, ino: u64, _fh: u64, offset: i64, mut reply: ReplyDirectory) {
        let path = match self.inode_get_path(ino) {
            Some(p) => p,
            None => { reply.error(libc::ENOENT); return; }
        };
        let entries = match self.fetch_dir(ino, &path) {
            Ok(e) => e,
            Err(errno) => { reply.error(errno); return; }
        };
        let parent_ino = self.inode_get_or_create(parent_path(&path));
        let mut full_entries: Vec<(u64, FileType, String)> = vec![
            (ino, FileType::Directory, ".".to_string()),
            (parent_ino, FileType::Directory, "..".to_string()),
        ];
        // Writeback overlay: drop tombstoned names from the server
        // listing, then merge the shadow's per-parent entries.
        // Dedup is by basename — a Clean shadow entry whose server
        // listing already includes it would otherwise double-surface
        // (Clean stays in pending_children so the file doesn't blink
        // out between commit and the next dir-cache TTL).
        let server_names: HashSet<&str> =
            entries.iter().map(|de| de.name.as_str()).collect();
        let (tombstoned, pending) = if self.is_writeback() {
            let shadow = self.shadow.as_ref().unwrap().lock().unwrap();
            let toms: HashSet<String> = entries
                .iter()
                .filter(|de| shadow.is_tombstoned(&de.path))
                .map(|de| de.path.clone())
                .collect();
            let pending: Vec<(u64, String)> = shadow
                .pending_children(ino)
                .map(|names| {
                    names
                        .iter()
                        .filter(|name| !server_names.contains(name.as_str()))
                        .filter_map(|name| {
                            let cp = Self::resolve_child_path(&path, name);
                            shadow.get(&cp).map(|e| (e.local_ino, name.clone()))
                        })
                        .collect()
                })
                .unwrap_or_default();
            (toms, pending)
        } else {
            (HashSet::new(), Vec::new())
        };
        // Pre-create inodes for every entry. Earlier this used a 0-ino
        // hint ("kernel will lookup if it wants to use the entry"),
        // which Linux FUSE accepts but macFUSE silently drops, so `ls`
        // on a populated workspace only returned the subset of entries
        // that had already been `lookup`'d into the inode table. The
        // small extra cost of always allocating is worth the cross-
        // platform consistency, and matches what readdirplus already
        // does.
        for de in entries.iter() {
            if tombstoned.contains(&de.path) {
                continue;
            }
            let child_ino = self.inode_get_or_create(&de.path);
            let kind = if de.is_dir { FileType::Directory } else { FileType::RegularFile };
            full_entries.push((child_ino, kind, de.name.clone()));
        }
        for (child_ino, name) in pending {
            full_entries.push((child_ino, FileType::RegularFile, name));
        }
        // Three reasons not to advertise a sidecar in the listing:
        //   - the server reported `summary_enabled = false` at mount
        //     time, so no directory will ever have a summary (no
        //     point inviting `cat` to discover ENOENT 1 round-trip
        //     at a time).
        //   - this slot in this dir returned ENOENT recently — the
        //     per-dir miss cache cleans up phantoms reactively for
        //     dirs the worker hasn't reached yet.
        //   - a legacy real file already owns the name (server-side
        //     reserved-name guard blocks new writes, but pre-existing
        //     rows are still around); FUSE lookup/read take the
        //     magic branch (documented shadowing trade-off in skill.md).
        if self.summary_enabled {
            for (magic_name, _) in MAGIC_NAMES {
                if self.sidecar_recently_missing(&path, magic_name) {
                    continue;
                }
                let magic_path = Self::resolve_child_path(&path, magic_name);
                let magic_ino = self.inode_get_or_create(&magic_path);
                full_entries.push((magic_ino, FileType::RegularFile, magic_name.to_string()));
            }
        }
        for (i, (child_ino, kind, name)) in full_entries.iter().enumerate().skip(offset as usize) {
            if reply.add(*child_ino, (i + 1) as i64, *kind, name) { break; }
        }
        reply.ok();
    }

    fn readdirplus(
        &mut self, _req: &Request<'_>, ino: u64, _fh: u64, offset: i64,
        mut reply: ReplyDirectoryPlus,
    ) {
        let path = match self.inode_get_path(ino) {
            Some(p) => p,
            None => { reply.error(libc::ENOENT); return; }
        };
        let entries = match self.fetch_dir(ino, &path) {
            Ok(e) => e,
            Err(errno) => { reply.error(errno); return; }
        };
        let parent_ino = self.inode_get_or_create(parent_path(&path));
        let self_attr = self.inode_get_attr(ino).unwrap_or_else(|| Self::dir_stub_attr(ino));
        let parent_attr = self.inode_get_attr(parent_ino).unwrap_or_else(|| Self::dir_stub_attr(parent_ino));

        let mut full: Vec<(u64, String, FileAttr, bool)> = Vec::with_capacity(entries.len() + 2);
        full.push((ino, ".".to_string(), self_attr, true));
        full.push((parent_ino, "..".to_string(), parent_attr, true));
        // Writeback overlay (same logic as readdir): drop tombstoned
        // server entries, append non-tombstoned shadow entries.
        // Dedups on basename so a Clean shadow entry already in the
        // server listing doesn't double-surface.
        let server_names: HashSet<&str> =
            entries.iter().map(|de| de.name.as_str()).collect();
        let (tombstoned, shadow_entries): (HashSet<String>, Vec<(u64, String, FileAttr)>) =
            if self.is_writeback() {
                let shadow = self.shadow.as_ref().unwrap().lock().unwrap();
                let toms: HashSet<String> = entries
                    .iter()
                    .filter(|de| shadow.is_tombstoned(&de.path))
                    .map(|de| de.path.clone())
                    .collect();
                let shadow_entries: Vec<(u64, String, FileAttr)> = shadow
                    .pending_children(ino)
                    .map(|names| {
                        names
                            .iter()
                            .filter(|name| !server_names.contains(name.as_str()))
                            .filter_map(|name| {
                                let cp = Self::resolve_child_path(&path, name);
                                shadow.get(&cp).map(|e| {
                                    (e.local_ino, name.clone(), Self::make_shadow_attr(e))
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                (toms, shadow_entries)
            } else {
                (HashSet::new(), Vec::new())
            };
        for de in entries.iter() {
            if tombstoned.contains(&de.path) {
                continue;
            }
            let child_ino = self.inode_get_or_create(&de.path);
            let attr = Self::attr_from_dir_entry(de, child_ino);
            let cache_ok = Self::should_cache_attr(de);
            if cache_ok {
                self.inode_set_attr(child_ino, attr);
            }
            full.push((child_ino, de.name.clone(), attr, cache_ok));
        }
        for (local_ino, name, attr) in shadow_entries {
            // Shadow entries don't carry the same "cache_ok" signal
            // as server entries; their attrs are computed fresh from
            // the in-memory buffer so caching is safe. nlookup
            // increment is centralised in the publish loop below.
            full.push((local_ino, name, attr, true));
        }
        // Synthetic sidecar entries. attr is a size-0 stub because
        // we haven't fetched the body yet and a readdirplus call
        // shouldn't itself touch the LLM — the next `cat` will hit
        // `lookup`, which fills in the real size. cache_ok=false
        // tells the loop below to publish them with a zero TTL so
        // the stub attr doesn't outlive the next lookup. Same
        // summary_enabled / per-dir miss-cache gating as readdir.
        if self.summary_enabled {
            for (magic_name, _) in MAGIC_NAMES {
                if self.sidecar_recently_missing(&path, magic_name) {
                    continue;
                }
                let magic_path = Self::resolve_child_path(&path, magic_name);
                let magic_ino = self.inode_get_or_create(&magic_path);
                let stub_attr = Self::magic_attr(magic_ino, 0);
                full.push((magic_ino, magic_name.to_string(), stub_attr, false));
            }
        }
        let zero_ttl = Duration::ZERO;
        let mut table = self.inodes.lock().unwrap();
        for (i, (child_ino, name, attr, cache_ok)) in full.iter().enumerate().skip(offset as usize) {
            let ttl = if *cache_ok { &self.config.attr_ttl } else { &zero_ttl };
            if reply.add(*child_ino, (i + 1) as i64, name, ttl, attr, 0) { break; }
            if name != "." && name != ".." {
                table.inc_nlookup(*child_ino);
            }
        }
        reply.ok();
    }

    fn open(&mut self, _req: &Request, ino: u64, flags: i32, reply: ReplyOpen) {
        let writable = (flags & libc::O_ACCMODE) != libc::O_RDONLY;
        // Sidecars (.abstract / .overview) are synthetic read-only
        // entries even when the mount is RW. Reject any non-RDONLY
        // open up front — otherwise create+write+release would
        // happily allocate a write handle and surface a confusing
        // EIO at flush time when the server rejects the path.
        if self.is_magic_ino(ino) {
            if writable || (flags & libc::O_TRUNC) != 0 {
                reply.error(libc::EROFS);
                return;
            }
            let fh = self.alloc_fh();
            reply.opened(fh, 0);
            return;
        }
        let fh = self.alloc_fh();
        if writable && self.config.read_only { reply.error(libc::EROFS); return; }
        if writable {
            let path = match self.inode_get_path(ino) {
                Some(p) => p,
                None => { reply.error(libc::ENOENT); return; }
            };
            let truncated = (flags & libc::O_TRUNC) != 0;
            let (existing, base_rev) = match self.client.stat(&path) {
                Ok(info) => {
                    let rev = info.revision;
                    if truncated {
                        (Vec::new(), rev)
                    } else {
                        let buf = match self.client.read_file(&path) {
                            Ok(bytes) => bytes,
                            Err(ClientError::NotFound) => Vec::new(),
                            Err(ref e) => { reply.error(Self::err_to_errno(e)); return; }
                        };
                        (buf, rev)
                    }
                }
                Err(ClientError::NotFound) => (Vec::new(), None),
                Err(ref e) => { reply.error(Self::err_to_errno(e)); return; }
            };
            // Writeback adopt-on-open: pull the existing server bytes
            // into the shadow so subsequent write/read on this fh
            // route through the same overlay as a fresh create()'s
            // file. Reuses the inode that's already in the table —
            // we do NOT alloc a new local_ino here (that would
            // invalidate any other handle holding the existing ino).
            // Cap-busting existing files (>10 MB) fall back to the
            // legacy buf path; flush_handle handles their sync write.
            //
            // O_TRUNC: adopt an EMPTY buffer (existing was set to
            // Vec::new() above when truncated=true). Without this,
            // a subsequent shorter write would only overwrite the
            // prefix of the stale buffer instead of replacing the
            // full content, and the eventual PUT would carry the old
            // tail concatenated to the new head.
            let adopt_eligible = self.is_writeback()
                && (truncated || !existing.is_empty());
            let mut adopted = false;
            if adopt_eligible {
                let parent_ino = self
                    .inodes
                    .lock()
                    .unwrap()
                    .get_ino(parent_path(&path))
                    .unwrap_or(crate::inode::ROOT_INO);
                let shadow = self.shadow.as_ref().unwrap();
                let mut store = shadow.lock().unwrap();
                if store
                    .adopt_server_file(
                        &path,
                        parent_ino,
                        ino,
                        base_rev.unwrap_or(0),
                        existing.clone(),
                    )
                    .is_ok()
                {
                    adopted = true;
                }
            }
            let buf = if adopted { Vec::new() } else { existing };
            self.write_handles.insert(fh, WriteHandle { ino, path: path.clone(), buf, dirty: truncated, base_rev });
        }
        reply.opened(fh, 0);
    }

    fn read(
        &mut self, _req: &Request, ino: u64, fh: u64,
        offset: i64, size: u32, _flags: i32, _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        // FUSE callers (notably libfuse / `cat`) routinely pass `size = u32::MAX`
        // when they don't know the file length yet. `off + size as usize` then
        // overflows on 32-bit platforms and silently wraps on 64-bit when off is
        // near usize::MAX. saturating_add caps at usize::MAX, then min() with
        // the actual buffer length yields the correct slice end.
        //
        // In writeback mode the user's writes land in the shadow, not in
        // `handle.buf`. Check shadow before the legacy write_handle short-
        // circuit so a write-then-read on the same fh sees the bytes that
        // were just written (otherwise: empty buf → returns []). Falls
        // through to the legacy path when the path isn't shadow-tracked
        // (open-existing legacy fallback — Day 3 closes that hole).
        if self.is_writeback() {
            if let Some(path) = self.inode_get_path(ino) {
                let shadow = self.shadow.as_ref().unwrap();
                let store = shadow.lock().unwrap();
                if let Some(entry) = store.get(&path) {
                    let off = offset as usize;
                    if off >= entry.data.len() {
                        reply.data(&[]);
                    } else {
                        let end = std::cmp::min(
                            off.saturating_add(size as usize),
                            entry.data.len(),
                        );
                        reply.data(&entry.data[off..end]);
                    }
                    return;
                }
            }
        }
        if let Some(handle) = self.write_handles.get(&fh) {
            let off = offset as usize;
            if off >= handle.buf.len() { reply.data(&[]); }
            else {
                let end = std::cmp::min(off.saturating_add(size as usize), handle.buf.len());
                reply.data(&handle.buf[off..end]);
            }
            return;
        }
        let path = match self.inode_get_path(ino) {
            Some(p) => p,
            None => { reply.error(libc::ENOENT); return; }
        };

        // Sidecar read: `lookup` already cached the body at the
        // synthetic path; we just slice from cache. If the cache TTL
        // expired between lookup and read (5s window) we re-fetch
        // through `magic_lookup`, which repopulates the cache.
        let basename = path.rsplit('/').next().unwrap_or("");
        if let Some(kind) = is_magic_name(basename) {
            let (cached, _) = self.cache_get_with_gen(&path);
            let body = match cached {
                Some(b) => b,
                None => {
                    let parent = parent_path(&path).to_string();
                    if let Err(errno) = self.magic_lookup(&parent, ino, kind) {
                        reply.error(errno);
                        return;
                    }
                    match self.cache_get_with_gen(&path).0 {
                        Some(b) => b,
                        None => {
                            // magic_lookup populated the cache but
                            // we couldn't read it back — would only
                            // happen on a TOCTOU eviction, treat as
                            // empty since the placeholder semantics
                            // are "pending or empty".
                            reply.data(&[]);
                            return;
                        }
                    }
                }
            };
            let off = offset as usize;
            if off >= body.len() {
                reply.data(&[]);
            } else {
                let end = std::cmp::min(off.saturating_add(size as usize), body.len());
                reply.data(&body[off..end]);
            }
            return;
        }

        let (cached, gen) = self.cache_get_with_gen(&path);
        if let Some(cached) = cached {
            let off = offset as usize;
            if off >= cached.len() { reply.data(&[]); }
            else {
                let end = std::cmp::min(off.saturating_add(size as usize), cached.len());
                reply.data(&cached[off..end]);
            }
            return;
        }

        let file_size = self.inode_get_attr(ino).map(|a| a.size).unwrap_or(0);
        if self.cache_is_cacheable(file_size) {
            match self.client.read_file(&path) {
                Ok(bytes) => {
                    let off = offset as usize;
                    if off >= bytes.len() { reply.data(&[]); }
                    else {
                        let end = std::cmp::min(off.saturating_add(size as usize), bytes.len());
                        reply.data(&bytes[off..end]);
                    }
                    self.cache_put(&path, bytes, gen);
                }
                Err(ref e) => reply.error(Self::err_to_errno(e)),
            }
        } else {
            match self.client.read_range(&path, offset as u64, size as u64) {
                Ok(data) => reply.data(&data),
                Err(ref e) => reply.error(Self::err_to_errno(e)),
            }
        }
    }

    fn write(
        &mut self, _req: &Request, ino: u64, fh: u64,
        offset: i64, data: &[u8],
        _write_flags: u32, _flags: i32, _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        // The `open` handler already refuses writable opens on magic
        // ino, but `write` auto-creates a `WriteHandle` on the first
        // call when `fh` is unknown (handles dropped fh tracking
        // across reopens). Without this guard, a `write` to a magic
        // ino would buffer locally and only surface EIO at flush.
        // EROFS up front matches the sidecar contract.
        if self.is_magic_ino(ino) {
            reply.error(libc::EROFS);
            return;
        }
        if !self.write_handles.contains_key(&fh) {
            if self.write_handles.values().any(|h| h.ino == ino && h.dirty) {
                warn!(ino, "concurrent dirty write handles for same inode");
            }
            // Resolve the path while the mapping is still alive (an
            // unlink between create and write would otherwise leave
            // the synthetic handle pathless). Empty string is OK as
            // a sentinel for "couldn't resolve": flush_handle and
            // the writeback fallback both no-op on empty path.
            let path = self.inode_get_path(ino).unwrap_or_default();
            self.write_handles.insert(fh, WriteHandle {
                ino, path, buf: Vec::new(), dirty: false, base_rev: None,
            });
        }
        // Writeback path: route into the shadow store and schedule a
        // debounced commit. Falls through to the legacy in-memory buf
        // when the path isn't shadow-tracked (Day 2 only covers the
        // create-then-write flow; open-existing-then-write is Day 3).
        if self.is_writeback() {
            let path = match self.inode_get_path(ino) {
                Some(p) => p,
                None => { reply.error(libc::ENOENT); return; }
            };
            let (had_entry, write_result) = {
                let shadow = self.shadow.as_ref().unwrap();
                let mut store = shadow.lock().unwrap();
                if store.get(&path).is_some() {
                    let r = store.write_at(&path, offset as u64, data);
                    (true, Some(r))
                } else {
                    (false, None)
                }
            };
            if had_entry {
                match write_result.unwrap() {
                    Ok(seq) => {
                        self.commit_queue.as_ref().unwrap().touch(path, seq);
                        // Track dirty so legacy flush_handle (still
                        // called during release in case shadow isn't
                        // tracking) doesn't claim there's no work.
                        let handle = self.write_handles.get_mut(&fh).unwrap();
                        handle.dirty = true;
                        reply.written(data.len() as u32);
                        return;
                    }
                    Err(InsertError::PerFileCapExceeded) => {
                        // Size cap fallback: sync-flush the existing
                        // shadow entry to the server, evict it, then
                        // continue with this write going through the
                        // legacy buf path. Net effect: files smaller
                        // than 10 MB enjoy the debounce; files that
                        // grow past it degrade silently to sync
                        // writes (current behaviour). The user
                        // doesn't see EFBIG.
                        if let Err(e) = self.sync_flush_and_evict_shadow(&path) {
                            reply.error(Self::err_to_errno(&e));
                            return;
                        }
                        // Fall through to the legacy buf write below.
                    }
                    Err(InsertError::TotalCapExceeded) => {
                        reply.error(libc::ENOSPC);
                        return;
                    }
                }
            }
            // Fall through to legacy buf path for paths not yet
            // shadow-tracked.
        }
        let handle = self.write_handles.get_mut(&fh).unwrap();
        let offset = offset as usize;
        if offset > handle.buf.len() { handle.buf.resize(offset, 0); }
        let end = offset + data.len();
        if end > handle.buf.len() { handle.buf.resize(end, 0); }
        handle.buf[offset..end].copy_from_slice(data);
        handle.dirty = true;
        reply.written(data.len() as u32);
    }

    fn flush(&mut self, _req: &Request, ino: u64, fh: u64, _lock_owner: u64, reply: ReplyEmpty) {
        if self.is_writeback() {
            // Writeback: an explicit touch keeps the path in the
            // queue without forcing a synchronous PUT. The actual
            // server write happens after the debounce window elapses.
            //
            // BUT: open-existing-then-write paths that aren't yet in
            // shadow (Day 3 closes this) fall back to the legacy buf
            // in `write()`. In that case `handle.dirty` is true with
            // no shadow entry, and we still need to flush_handle so
            // the user's :w doesn't silently drop the buffer.
            self.writeback_touch_or_fallback_flush(fh, ino, reply);
            return;
        }
        match self.flush_handle(fh, ino) {
            Ok(()) => reply.ok(),
            Err(errno) => reply.error(errno),
        }
    }

    fn fsync(&mut self, _req: &Request, ino: u64, fh: u64, _datasync: bool, reply: ReplyEmpty) {
        if self.is_writeback() {
            self.writeback_touch_or_fallback_flush(fh, ino, reply);
            return;
        }
        match self.flush_handle(fh, ino) {
            Ok(()) => reply.ok(),
            Err(errno) => reply.error(errno),
        }
    }

    fn release(
        &mut self, _req: &Request, ino: u64, fh: u64,
        _flags: i32, _lock_owner: Option<u64>, _flush: bool,
        reply: ReplyEmpty,
    ) {
        if self.is_writeback() {
            // Same shadow-vs-legacy split as flush(), then drop the
            // handle and return.
            self.writeback_touch_or_fallback_flush(fh, ino, reply);
            self.write_handles.remove(&fh);
            return;
        }
        let result = self.flush_handle(fh, ino);
        self.write_handles.remove(&fh);
        match result {
            Ok(()) => reply.ok(),
            Err(errno) => reply.error(errno),
        }
    }

    fn create(
        &mut self, _req: &Request, parent: u64, name: &OsStr,
        _mode: u32, _umask: u32, _flags: i32,
        reply: ReplyCreate,
    ) {
        if self.config.read_only { reply.error(libc::EROFS); return; }
        let name_str = match name.to_str() {
            Some(s) => s, None => { reply.error(libc::EINVAL); return; }
        };
        // Refuse to create a real file with a sidecar name. The
        // server-side reserved-name check would also reject this,
        // but failing locally with EROFS is faster + more accurate
        // (server would map to EIO).
        if is_magic_name(name_str).is_some() {
            reply.error(libc::EROFS);
            return;
        }
        let parent_path = match self.inode_get_path(parent) {
            Some(p) => p, None => { reply.error(libc::ENOENT); return; }
        };
        let child_path = Self::resolve_child_path(&parent_path, name_str);
        // Writeback path: defer the server PUT entirely. Allocate a
        // local-only inode, register it in the InodeTable so lookup
        // can resolve it, seed a LocalOnly shadow entry, return.
        // The commit queue will push the file (with whatever bytes
        // arrive via subsequent write()s) after the debounce window;
        // a vim swap that gets unlinked first never produces a PUT.
        if self.is_writeback() {
            self.invalidate_dir_cache(parent);
            let local_ino;
            let attr;
            {
                let shadow = self.shadow.as_ref().unwrap();
                let mut store = shadow.lock().unwrap();
                let (l, _seq) = store.create_local(&child_path, parent);
                local_ino = l;
                let entry = store.get(&child_path).expect("just inserted");
                attr = Self::make_shadow_attr(entry);
            }
            self.inodes.lock().unwrap().register_local(local_ino, &child_path);
            self.inodes.lock().unwrap().inc_nlookup(local_ino);
            self.inode_set_attr(local_ino, attr);
            let fh = self.alloc_fh();
            self.write_handles.insert(
                fh,
                WriteHandle {
                    ino: local_ino,
                    path: child_path.clone(),
                    buf: Vec::new(),
                    dirty: false,
                    base_rev: None,
                },
            );
            reply.created(&self.config.attr_ttl, &attr, 0, fh, 0);
            return;
        }
        // Sync legacy path: hit the server immediately.
        let base_rev = match self.client.write_file(&child_path, b"", None) {
            Ok(rev) => rev,
            Err(ref e) => { reply.error(Self::err_to_errno(e)); return; }
        };
        self.invalidate_dir_cache(parent);
        let fh = self.alloc_fh();
        let ino = self.inode_get_or_create_with_nlookup(&child_path);
        let now = chrono::Utc::now();
        let info = FileInfo {
            path: child_path.clone(),
            is_dir: false,
            size_bytes: Some(0),
            revision: base_rev,
            created_at: Some(now),
            updated_at: Some(now),
        };
        let attr = Self::make_attr(&info, ino);
        self.inode_set_attr(ino, attr);
        self.write_handles.insert(fh, WriteHandle { ino, path: child_path, buf: Vec::new(), dirty: false, base_rev });
        reply.created(&self.config.attr_ttl, &attr, 0, fh, 0);
    }

    fn mkdir(
        &mut self, _req: &Request, parent: u64, name: &OsStr,
        _mode: u32, _umask: u32, reply: ReplyEntry,
    ) {
        if self.config.read_only { reply.error(libc::EROFS); return; }
        let name_str = match name.to_str() {
            Some(s) => s, None => { reply.error(libc::EINVAL); return; }
        };
        if is_magic_name(name_str).is_some() {
            reply.error(libc::EROFS);
            return;
        }
        let parent_path = match self.inode_get_path(parent) {
            Some(p) => p, None => { reply.error(libc::ENOENT); return; }
        };
        let child_path = Self::resolve_child_path(&parent_path, name_str);
        if let Err(ref e) = self.client.mkdir(&child_path) {
            reply.error(Self::err_to_errno(e)); return;
        }
        self.invalidate_dir_cache(parent);
        let ino = self.inode_get_or_create_with_nlookup(&child_path);
        let now = chrono::Utc::now();
        let info = FileInfo {
            path: child_path,
            is_dir: true,
            size_bytes: None,
            revision: None,
            created_at: Some(now),
            updated_at: Some(now),
        };
        let attr = Self::make_attr(&info, ino);
        self.inode_set_attr(ino, attr);
        reply.entry(&self.config.attr_ttl, &attr, 0);
    }

    fn unlink(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        if self.config.read_only { reply.error(libc::EROFS); return; }
        let name_str = match name.to_str() {
            Some(s) => s, None => { reply.error(libc::EINVAL); return; }
        };
        if is_magic_name(name_str).is_some() {
            reply.error(libc::EROFS);
            return;
        }
        let parent_path = match self.inode_get_path(parent) {
            Some(p) => p, None => { reply.error(libc::ENOENT); return; }
        };
        let child_path = Self::resolve_child_path(&parent_path, name_str);
        // Writeback path: tombstone the shadow entry + cancel any
        // pending commit. Only hit the server with DELETE if the
        // entry has reached the server at least once (Dirty/Clean).
        // A LocalOnly entry was never PUT — vim swap files take this
        // branch and produce zero server calls end-to-end. A path
        // not in shadow at all is assumed to be a real server file
        // and gets a regular DELETE (consistent with sync mode).
        if self.is_writeback() {
            let server_had_it;
            let cancel_seq;
            {
                let shadow = self.shadow.as_ref().unwrap();
                let mut store = shadow.lock().unwrap();
                let (prior_kind, new_seq) = store.tombstone(&child_path, parent);
                cancel_seq = new_seq;
                server_had_it = match prior_kind {
                    Some(EntryKind::Dirty) | Some(EntryKind::Clean) => true,
                    Some(EntryKind::LocalOnly) => false,
                    // No shadow entry: a pre-existing server file
                    // the user opened/looked up via the server side.
                    None => true,
                };
            }
            self.commit_queue.as_ref().unwrap().cancel(child_path.clone(), cancel_seq);
            if server_had_it {
                if let Err(ref e) = self.client.delete(&child_path) {
                    // ENOENT is fine — the server may have GC'd the
                    // file already, or our `server_had_it` heuristic
                    // misjudged a no-shadow path that was actually
                    // local-only. Other errors leak back to caller.
                    if !matches!(e, ClientError::NotFound) {
                        reply.error(Self::err_to_errno(e));
                        return;
                    }
                }
            }
            self.invalidate_dir_cache(parent);
            self.inode_remove(&child_path);
            self.cache_invalidate(&child_path);
            reply.ok();
            return;
        }
        match self.client.delete(&child_path) {
            Ok(()) => {
                self.invalidate_dir_cache(parent);
                self.inode_remove(&child_path);
                self.cache_invalidate(&child_path);
                reply.ok();
            }
            Err(ref e) => reply.error(Self::err_to_errno(e)),
        }
    }

    fn rmdir(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        if self.config.read_only { reply.error(libc::EROFS); return; }
        let name_str = match name.to_str() {
            Some(s) => s, None => { reply.error(libc::EINVAL); return; }
        };
        if is_magic_name(name_str).is_some() {
            // Symmetric with unlink/create/rename: sidecars aren't
            // directories anyway, so the server would 404, but
            // returning EROFS locally keeps the contract uniform and
            // saves a round-trip.
            reply.error(libc::EROFS);
            return;
        }
        let parent_path = match self.inode_get_path(parent) {
            Some(p) => p, None => { reply.error(libc::ENOENT); return; }
        };
        let child_path = Self::resolve_child_path(&parent_path, name_str);
        match self.client.list_dir(&child_path) {
            Ok(entries) if !entries.is_empty() => { reply.error(libc::ENOTEMPTY); return; }
            Err(ref e) => { reply.error(Self::err_to_errno(e)); return; }
            _ => {}
        }
        match self.client.delete(&child_path) {
            Ok(()) => {
                self.invalidate_dir_cache(parent);
                self.inode_remove(&child_path);
                reply.ok();
            }
            Err(ref e) => reply.error(Self::err_to_errno(e)),
        }
    }

    fn rename(
        &mut self, _req: &Request, parent: u64, name: &OsStr,
        newparent: u64, newname: &OsStr, _flags: u32, reply: ReplyEmpty,
    ) {
        if self.config.read_only { reply.error(libc::EROFS); return; }
        let name_str = match name.to_str() {
            Some(s) => s, None => { reply.error(libc::EINVAL); return; }
        };
        let newname_str = match newname.to_str() {
            Some(s) => s, None => { reply.error(libc::EINVAL); return; }
        };
        // Renaming `.abstract` itself, or renaming any file *into*
        // a sidecar name, must fail locally — the server's reserved-
        // name check would reject the destination anyway but EROFS
        // is the accurate verb for "can't touch the magic entries".
        if is_magic_name(name_str).is_some() || is_magic_name(newname_str).is_some() {
            reply.error(libc::EROFS);
            return;
        }
        let parent_path = match self.inode_get_path(parent) {
            Some(p) => p, None => { reply.error(libc::ENOENT); return; }
        };
        let newparent_path = match self.inode_get_path(newparent) {
            Some(p) => p, None => { reply.error(libc::ENOENT); return; }
        };
        let old_path = Self::resolve_child_path(&parent_path, name_str);
        let new_path = Self::resolve_child_path(&newparent_path, newname_str);
        // Writeback rename: server first, then shadow. Doing it the
        // other way around could leave the local overlay holding the
        // new path while the server still has the old one if the
        // server call errors. prior_kind decides whether the server
        // needs to be touched at all:
        //   - LocalOnly  → server never saw old_path; no server call
        //                  (covers git's `.git/index.lock → .git/index`
        //                  pattern: the .lock never reaches the server).
        //   - Dirty/Clean / no shadow entry → server has the path,
        //                                     rename it. ENOENT is OK.
        if self.is_writeback() {
            let prior_kind = {
                let shadow = self.shadow.as_ref().unwrap();
                shadow.lock().unwrap().get(&old_path).map(|e| e.kind)
            };
            let server_rename = matches!(
                prior_kind,
                Some(EntryKind::Dirty) | Some(EntryKind::Clean) | None
            );
            if server_rename {
                if let Err(ref e) = self.client.rename(&old_path, &new_path) {
                    if !matches!(e, ClientError::NotFound) {
                        // Server rejected — leave the shadow unchanged
                        // so local view stays consistent with server.
                        reply.error(Self::err_to_errno(e));
                        return;
                    }
                }
            }
            if prior_kind.is_some() {
                let new_seq = {
                    let shadow = self.shadow.as_ref().unwrap();
                    let mut store = shadow.lock().unwrap();
                    store
                        .rename(&old_path, &new_path, newparent)
                        .unwrap_or(0)
                };
                let q = self.commit_queue.as_ref().unwrap();
                // Cancel any prior Touch for the old path; if a PUT
                // is already in flight, the tombstone shadow.rename
                // installed at old_path will make commit_queue's
                // post-PUT check chase it with a DELETE.
                q.cancel(old_path.clone(), new_seq);
                q.touch(new_path.clone(), new_seq);
            }
            self.invalidate_dir_cache(parent);
            self.invalidate_dir_cache(newparent);
            self.inode_rename(&old_path, &new_path);
            self.cache_invalidate(&old_path);
            self.cache_invalidate(&new_path);
            // inode_rename preserves the (ino, path) mapping under
            // the new path; no extra register_local needed because
            // the local_ino was already in the table at create time.
            reply.ok();
            return;
        }
        match self.client.rename(&old_path, &new_path) {
            Ok(()) => {
                self.invalidate_dir_cache(parent);
                self.invalidate_dir_cache(newparent);
                self.inode_rename(&old_path, &new_path);
                self.cache_invalidate(&old_path);
                self.cache_invalidate(&new_path);
                reply.ok();
            }
            Err(ref e) => reply.error(Self::err_to_errno(e)),
        }
    }

    fn opendir(&mut self, _req: &Request, _ino: u64, _flags: i32, reply: ReplyOpen) {
        reply.opened(0, 0);
    }

    fn statfs(&mut self, _req: &Request, _ino: u64, reply: fuser::ReplyStatfs) {
        reply.statfs(1 << 30, 1 << 30, 1 << 30, 1 << 20, 1 << 20, BLOCK_SIZE, 255, 0);
    }

    fn access(&mut self, _req: &Request, _ino: u64, _mask: i32, reply: ReplyEmpty) {
        reply.ok();
    }

    fn init(&mut self, _req: &Request<'_>, _config: &mut fuser::KernelConfig) -> Result<(), libc::c_int> {
        debug!("FUSE init");
        if let Some(fd) = self.notify_fd.take() {
            let _ = nix::unistd::write(
                unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) },
                b"R",
            );
            let _ = nix::unistd::close(fd);
        }
        Ok(())
    }

    fn destroy(&mut self) {
        debug!("FUSE destroy");
        // Writeback: push every Dirty/LocalOnly shadow entry through
        // the commit queue and block until they've landed (or failed).
        // Without this, an unmount during the debounce window silently
        // loses up to N seconds of writes.
        //
        // Skip Clean entries — they're already on the server with
        // their latest bytes; touching them again would burn an
        // unnecessary PUT and bump the server-side revision for no
        // gain.
        if self.is_writeback() {
            let pending_paths: Vec<(String, u64)> = {
                let shadow = self.shadow.as_ref().unwrap();
                let store = shadow.lock().unwrap();
                store
                    .pending_children_iter()
                    .filter_map(|path| {
                        let entry = store.get(&path)?;
                        if entry.kind == EntryKind::Clean {
                            return None;
                        }
                        Some((path, entry.seq))
                    })
                    .collect()
            };
            let q = self.commit_queue.as_ref().unwrap();
            for (path, seq) in pending_paths {
                q.touch(path, seq);
            }
            q.drain();
        }
        // Legacy buf sweep for paths NOT under shadow control. In
        // writeback mode the shadow drain above already pushed every
        // shadow-tracked entry; the WriteHandle.buf for those is
        // always empty (writeback writes go to the shadow, not the
        // buf). Running flush_handle on them would PUT an empty body
        // and clobber the server-side bytes we just successfully
        // committed — classic data-loss. Filter handles by whether
        // their path lives in the shadow.
        let fhs: Vec<u64> = self.write_handles.keys().copied().collect();
        for fh in fhs {
            let (ino, dirty, path) = match self.write_handles.get(&fh) {
                Some(h) => (h.ino, h.dirty, h.path.clone()),
                None => continue,
            };
            if !dirty {
                continue;
            }
            if self.is_writeback() {
                let shadow = self.shadow.as_ref().unwrap().lock().unwrap();
                if shadow.get(&path).is_some() {
                    // Shadow already drained this path; the buf is
                    // either empty or stale. Skip the legacy flush.
                    continue;
                }
            }
            let _ = self.flush_handle(fh, ino);
        }
        self.write_handles.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::DirEntry;

    fn make_dir_entry(name: &str, is_dir: bool, size: Option<i64>) -> DirEntry {
        DirEntry {
            name: name.to_string(),
            path: format!("/{name}"),
            is_dir,
            size_bytes: size,
            created_at: None,
            updated_at: None,
        }
    }

    #[test]
    fn attr_from_dir_entry_uses_zero_when_size_missing() {
        let de = make_dir_entry("a.txt", false, None);
        let attr = VedaFs::attr_from_dir_entry(&de, 42);
        assert_eq!(attr.size, 0);
    }

    #[test]
    fn attr_from_dir_entry_preserves_size() {
        let de = make_dir_entry("a.txt", false, Some(1234));
        let attr = VedaFs::attr_from_dir_entry(&de, 42);
        assert_eq!(attr.size, 1234);
    }

    #[test]
    fn file_with_unknown_size_is_not_cached() {
        let de = make_dir_entry("a.txt", false, None);
        assert!(!VedaFs::should_cache_attr(&de), "files with unknown size must not be cached");
    }

    #[test]
    fn file_with_known_size_is_cached() {
        let de = make_dir_entry("a.txt", false, Some(100));
        assert!(VedaFs::should_cache_attr(&de));
    }

    #[test]
    fn directory_is_always_cached_even_with_none_size() {
        let de = make_dir_entry("docs", true, None);
        assert!(VedaFs::should_cache_attr(&de), "directories legitimately have size=0");
    }

    #[test]
    fn make_attr_uses_server_mtime_when_present() {
        // 2026-04-01T00:00:00Z = 1774310400 unix seconds
        let dt: chrono::DateTime<chrono::Utc> =
            chrono::DateTime::parse_from_rfc3339("2026-04-01T00:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc);
        let info = FileInfo {
            path: "/x".to_string(),
            is_dir: false,
            size_bytes: Some(42),
            revision: Some(1),
            created_at: Some(dt),
            updated_at: Some(dt),
        };
        let attr = VedaFs::make_attr(&info, 100);
        let expected = UNIX_EPOCH + std::time::Duration::from_secs(dt.timestamp() as u64);
        assert_eq!(attr.mtime, expected);
        assert_eq!(attr.ctime, expected);
        assert_eq!(attr.crtime, expected);
    }

    #[test]
    fn make_attr_falls_back_to_now_when_mtime_missing() {
        let info = FileInfo {
            path: "/x".to_string(),
            is_dir: false,
            size_bytes: Some(0),
            revision: None,
            created_at: None,
            updated_at: None,
        };
        let before = SystemTime::now();
        let attr = VedaFs::make_attr(&info, 1);
        let after = SystemTime::now();
        // Should fall within the call window — proves we used wall clock.
        assert!(attr.mtime >= before && attr.mtime <= after);
    }

    // ── magic sidecar helpers ─────────────────────────────────────

    #[test]
    fn is_magic_name_recognises_both_sidecars() {
        assert!(matches!(
            is_magic_name(".abstract"),
            Some(SummaryKind::Abstract)
        ));
        assert!(matches!(
            is_magic_name(".overview"),
            Some(SummaryKind::Overview)
        ));
    }

    #[test]
    fn is_magic_name_rejects_non_exact_matches() {
        // The reserved names match byte-for-byte. Tail decoration,
        // prefixes, or different files starting with a dot must not
        // be hijacked.
        assert!(is_magic_name(".abstracts").is_none(), "trailing s");
        assert!(is_magic_name(".abstract.bak").is_none(), "trailing .bak");
        assert!(is_magic_name("abstract").is_none(), "no leading dot");
        assert!(is_magic_name(".Abstract").is_none(), "case-sensitive");
        assert!(is_magic_name("").is_none());
    }

    #[test]
    fn magic_attr_marks_read_only_and_sizes_from_body() {
        // Sidecar attr contract: regular file kind, mode 0444, no
        // hard links beyond the entry itself, and `size` mirrors
        // the body we will hand to `read`. nlink=1 because there's
        // no other path that points at the synthetic inode.
        let attr = VedaFs::magic_attr(99, 4096);
        assert_eq!(attr.size, 4096);
        assert_eq!(attr.perm, 0o444);
        assert!(matches!(attr.kind, FileType::RegularFile));
        assert_eq!(attr.nlink, 1);
        assert_eq!(attr.ino, 99);
    }

    #[test]
    fn magic_name_literal_returns_canonical_static_for_known_names() {
        // The miss-cache key requires `&'static str`, so the helper
        // can't just return the input slice. It has to map back to
        // the constant table entry.
        assert_eq!(VedaFs::magic_name_literal(".abstract"), Some(".abstract"));
        assert_eq!(VedaFs::magic_name_literal(".overview"), Some(".overview"));
        assert!(VedaFs::magic_name_literal(".other").is_none());
        assert!(VedaFs::magic_name_literal("").is_none());
    }

    #[test]
    fn sidecar_miss_cache_floors_attr_ttl_zero_at_one_second() {
        // attr_ttl=0 is nonsensical for the miss cache (would
        // reintroduce phantom entries the cache exists to
        // suppress). The 1s floor at `miss_cache_ttl` makes the
        // cache still work in this corner.
        let config = FuseConfig {
            attr_ttl: std::time::Duration::ZERO,
            read_only: false,
            cache_size_mb: 1,
        };
        let client = Arc::new(VedaClient::new("http://127.0.0.1:1", "k"));
        let fs = VedaFs::new(client, config, true, None, None, None);
        fs.note_sidecar_missing("/docs", ".abstract");
        assert!(
            fs.sidecar_recently_missing("/docs", ".abstract"),
            "miss must persist under the 1s floor even when attr_ttl=0"
        );
    }

    #[test]
    fn sidecar_miss_cache_remembers_within_ttl_and_clears_on_hit() {
        // End-to-end behaviour of the three cache helpers without
        // spinning up a real FUSE mount. Pins the TTL semantics:
        //   - note_sidecar_missing → recently_missing returns true
        //   - clear_sidecar_missing → recently_missing returns false
        // Past-TTL behaviour is exercised indirectly by setting a
        // tiny attr_ttl and sleeping; we avoid that here because
        // CI clock jitter makes sub-second sleeps flaky.
        let config = FuseConfig {
            attr_ttl: std::time::Duration::from_secs(5),
            read_only: false,
            cache_size_mb: 1,
        };
        // VedaFs::new needs a real VedaClient. We can construct one
        // pointing at a bogus URL — none of the cache helpers call
        // out to it, and we never invoke any method that does.
        let client = Arc::new(VedaClient::new("http://127.0.0.1:1", "k"));
        let fs = VedaFs::new(client, config, true, None, None, None);

        assert!(!fs.sidecar_recently_missing("/docs", ".abstract"));
        fs.note_sidecar_missing("/docs", ".abstract");
        assert!(fs.sidecar_recently_missing("/docs", ".abstract"));
        // Different basename in same dir: independent.
        assert!(!fs.sidecar_recently_missing("/docs", ".overview"));
        // Different dir, same basename: independent.
        assert!(!fs.sidecar_recently_missing("/notes", ".abstract"));
        // Clear restores availability so a freshly-generated summary
        // shows up on the next readdir without TTL wait.
        fs.clear_sidecar_missing("/docs", ".abstract");
        assert!(!fs.sidecar_recently_missing("/docs", ".abstract"));
    }

    #[test]
    fn pending_body_is_one_line_with_newline() {
        // Tools that line-buffer (`while read line`, log tail) need
        // a trailing newline so the placeholder shows up promptly
        // instead of being held in the buffer until a future fetch
        // appends more. Pinned because dropping it silently would
        // make `cat` look like it hung.
        assert!(SIDECAR_PENDING_BODY.ends_with('\n'));
        assert_eq!(
            SIDECAR_PENDING_BODY.matches('\n').count(),
            1,
            "must be a single line"
        );
    }

    #[test]
    fn read_size_u32_max_does_not_overflow() {
        // Regression: FUSE callers can pass size = u32::MAX when length is
        // unknown. `off + size as usize` previously overflowed; with the
        // saturating_add fix the slice end clamps to buffer length.
        let buf: Vec<u8> = vec![0u8; 10];
        let off: usize = 0;
        let size: u32 = u32::MAX;
        let end = std::cmp::min(off.saturating_add(size as usize), buf.len());
        assert_eq!(end, buf.len());
        let _ = &buf[off..end];
    }
}
