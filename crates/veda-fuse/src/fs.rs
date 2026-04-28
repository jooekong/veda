use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::os::fd::RawFd;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use fuser::{
    FileAttr, FileType, Filesystem, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory,
    ReplyDirectoryPlus, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite, Request,
};
use tracing::{debug, warn};

use crate::cache::ReadCache;
use crate::client::{ClientError, DirEntry, FileInfo, VedaClient};
use crate::inode::InodeTable;

const BLOCK_SIZE: u32 = 512;

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

    fn make_attr(info: &FileInfo, ino: u64) -> FileAttr {
        let kind = if info.is_dir { FileType::Directory } else { FileType::RegularFile };
        let size = info.size_bytes.unwrap_or(0) as u64;
        let perm = if info.is_dir { 0o755 } else { 0o644 };
        let now = SystemTime::now();
        FileAttr {
            ino, size,
            blocks: (size + BLOCK_SIZE as u64 - 1) / BLOCK_SIZE as u64,
            atime: now, mtime: now, ctime: now, crtime: now,
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
        };
        Self::make_attr(&info, ino)
    }

    fn should_cache_attr(de: &DirEntry) -> bool {
        de.is_dir || de.size_bytes.is_some()
    }

    fn dir_stub_attr(ino: u64) -> FileAttr {
        let info = FileInfo { path: String::new(), is_dir: true, size_bytes: None, revision: None };
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
        // ino=0 means "no hint": kernel will issue lookup if it wants to use
        // the entry, which is the path that properly tracks nlookup.
        let table = self.inodes.lock().unwrap();
        for de in entries.iter() {
            let hint_ino = table.get_ino(&de.path).unwrap_or(0);
            let kind = if de.is_dir { FileType::Directory } else { FileType::RegularFile };
            full_entries.push((hint_ino, kind, de.name.clone()));
        }
        drop(table);
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
        let fh = self.alloc_fh();
        let writable = (flags & libc::O_ACCMODE) != libc::O_RDONLY;
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
        if let Some(handle) = self.write_handles.get(&fh) {
            let off = offset as usize;
            if off >= handle.buf.len() { reply.data(&[]); }
            else {
                let end = std::cmp::min(off + size as usize, handle.buf.len());
                reply.data(&handle.buf[off..end]);
            }
            return;
        }
        let path = match self.inode_get_path(ino) {
            Some(p) => p,
            None => { reply.error(libc::ENOENT); return; }
        };

        let (cached, gen) = self.cache_get_with_gen(&path);
        if let Some(cached) = cached {
            let off = offset as usize;
            if off >= cached.len() { reply.data(&[]); }
            else {
                let end = std::cmp::min(off + size as usize, cached.len());
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
                        let end = std::cmp::min(off + size as usize, bytes.len());
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
        let info = FileInfo { path: child_path, is_dir: false, size_bytes: Some(0), revision: base_rev };
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
        let parent_path = match self.inode_get_path(parent) {
            Some(p) => p, None => { reply.error(libc::ENOENT); return; }
        };
        let child_path = Self::resolve_child_path(&parent_path, name_str);
        if let Err(ref e) = self.client.mkdir(&child_path) {
            reply.error(Self::err_to_errno(e)); return;
        }
        self.invalidate_dir_cache(parent);
        let ino = self.inode_get_or_create_with_nlookup(&child_path);
        let info = FileInfo { path: child_path, is_dir: true, size_bytes: None, revision: None };
        let attr = Self::make_attr(&info, ino);
        self.inode_set_attr(ino, attr);
        reply.entry(&self.config.attr_ttl, &attr, 0);
    }

    fn unlink(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        if self.config.read_only { reply.error(libc::EROFS); return; }
        let name_str = match name.to_str() {
            Some(s) => s, None => { reply.error(libc::EINVAL); return; }
        };
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
}
