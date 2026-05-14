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
use crate::inode::InodeTable;

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

pub struct VedaFs {
    client: VedaClient,
    inodes: Arc<Mutex<InodeTable>>,
    config: FuseConfig,
    read_cache: Arc<Mutex<ReadCache>>,
    next_fh: u64,
    write_handles: HashMap<u64, WriteHandle>,
    dir_cache: DirCacheMap,
    /// File descriptor to signal parent process that mount succeeded (daemon mode).
    notify_fd: Option<RawFd>,
}

impl VedaFs {
    pub fn new(client: VedaClient, config: FuseConfig, notify_fd: Option<RawFd>) -> Self {
        let read_cache = Arc::new(Mutex::new(ReadCache::new(config.cache_size_mb, config.attr_ttl)));
        let inodes = Arc::new(Mutex::new(InodeTable::new_with_ttl(config.attr_ttl)));
        let dir_cache: DirCacheMap = Arc::new(Mutex::new(HashMap::new()));
        Self { client, inodes, config, read_cache, next_fh: 1, write_handles: HashMap::new(), dir_cache, notify_fd }
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
        let body = match outcome {
            SidecarOutcome::Body(b) => b,
            SidecarOutcome::Pending => SIDECAR_PENDING_BODY.as_bytes().to_vec(),
            SidecarOutcome::Disabled | SidecarOutcome::NotFound => return Err(libc::ENOENT),
        };
        let size = body.len() as u64;
        // Stash the body in the regular read cache so the
        // immediately-following `read` doesn't make a second HTTP
        // call. The key is the synthetic sidecar path (e.g.
        // `/docs/.abstract`); the server-side reserved-name check
        // prevents a real file from sharing the key.
        let sidecar_path = match kind {
            SummaryKind::Abstract => format!("{}/.abstract", target_path.trim_end_matches('/')),
            SummaryKind::Overview => format!("{}/.overview", target_path.trim_end_matches('/')),
        };
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
        // Pre-create inodes for every entry. Earlier this used a 0-ino
        // hint ("kernel will lookup if it wants to use the entry"),
        // which Linux FUSE accepts but macFUSE silently drops, so `ls`
        // on a populated workspace only returned the subset of entries
        // that had already been `lookup`'d into the inode table. The
        // small extra cost of always allocating is worth the cross-
        // platform consistency, and matches what readdirplus already
        // does.
        for de in entries.iter() {
            let child_ino = self.inode_get_or_create(&de.path);
            let kind = if de.is_dir { FileType::Directory } else { FileType::RegularFile };
            full_entries.push((child_ino, kind, de.name.clone()));
        }
        // Append the synthetic summary sidecars at the end of every
        // directory listing. A legacy file with the same name would
        // appear twice in `ls -a` (real file + synthetic entry) but
        // FUSE lookup/read still take the magic branch (see lookup
        // for the documented shadowing trade-off). New writes can't
        // create the collision — server-side reserved-name guard at
        // veda-server/src/routes/fs.rs blocks them.
        for (magic_name, _) in MAGIC_NAMES {
            let magic_path = Self::resolve_child_path(&path, magic_name);
            let magic_ino = self.inode_get_or_create(&magic_path);
            full_entries.push((magic_ino, FileType::RegularFile, magic_name.to_string()));
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
        for de in entries.iter() {
            let child_ino = self.inode_get_or_create(&de.path);
            let attr = Self::attr_from_dir_entry(de, child_ino);
            let cache_ok = Self::should_cache_attr(de);
            if cache_ok {
                self.inode_set_attr(child_ino, attr);
            }
            full.push((child_ino, de.name.clone(), attr, cache_ok));
        }
        // Synthetic sidecar entries. attr is a size-0 stub because
        // we haven't fetched the body yet and a readdirplus call
        // shouldn't itself touch the LLM — the next `cat` will hit
        // `lookup`, which fills in the real size. cache_ok=false
        // tells the loop below to publish them with a zero TTL so
        // the stub attr doesn't outlive the next lookup. See
        // readdir for the legacy-collision shadowing note.
        for (magic_name, _) in MAGIC_NAMES {
            let magic_path = Self::resolve_child_path(&path, magic_name);
            let magic_ino = self.inode_get_or_create(&magic_path);
            let stub_attr = Self::magic_attr(magic_ino, 0);
            full.push((magic_ino, magic_name.to_string(), stub_attr, false));
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
            self.write_handles.insert(fh, WriteHandle { ino, buf: existing, dirty: truncated, base_rev });
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
            self.write_handles.insert(fh, WriteHandle {
                ino, buf: Vec::new(), dirty: false, base_rev: None,
            });
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
        match self.flush_handle(fh, ino) {
            Ok(()) => reply.ok(),
            Err(errno) => reply.error(errno),
        }
    }

    fn fsync(&mut self, _req: &Request, ino: u64, fh: u64, _datasync: bool, reply: ReplyEmpty) {
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
        let base_rev = match self.client.write_file(&child_path, b"", None) {
            Ok(rev) => rev,
            Err(ref e) => { reply.error(Self::err_to_errno(e)); return; }
        };
        self.invalidate_dir_cache(parent);
        let fh = self.alloc_fh();
        let ino = self.inode_get_or_create_with_nlookup(&child_path);
        let now = chrono::Utc::now();
        let info = FileInfo {
            path: child_path,
            is_dir: false,
            size_bytes: Some(0),
            revision: base_rev,
            created_at: Some(now),
            updated_at: Some(now),
        };
        let attr = Self::make_attr(&info, ino);
        self.inode_set_attr(ino, attr);
        self.write_handles.insert(fh, WriteHandle { ino, buf: Vec::new(), dirty: false, base_rev });
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
        let fhs: Vec<u64> = self.write_handles.keys().copied().collect();
        for fh in fhs {
            if let Some(handle) = self.write_handles.get(&fh) {
                if handle.dirty {
                    let ino = handle.ino;
                    let _ = self.flush_handle(fh, ino);
                }
            }
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
