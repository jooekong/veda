use std::collections::HashMap;
use std::ffi::OsStr;
use std::os::fd::RawFd;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use fuser::{
    FileAttr, FileType, Filesystem, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty,
    ReplyEntry, ReplyOpen, ReplyWrite, Request,
};
use tracing::{debug, warn};

use crate::cache::ReadCache;
use crate::client::{ClientError, FileInfo, VedaClient};
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

pub struct VedaFs {
    client: VedaClient,
    inodes: Arc<Mutex<InodeTable>>,
    config: FuseConfig,
    read_cache: Arc<Mutex<ReadCache>>,
    next_fh: u64,
    write_handles: HashMap<u64, WriteHandle>,
    /// File descriptor to signal parent process that mount succeeded (daemon mode).
    notify_fd: Option<RawFd>,
}

impl VedaFs {
    pub fn new(client: VedaClient, config: FuseConfig, notify_fd: Option<RawFd>) -> Self {
        let read_cache = Arc::new(Mutex::new(ReadCache::new(config.cache_size_mb)));
        let inodes = Arc::new(Mutex::new(InodeTable::new_with_ttl(config.attr_ttl)));
        Self { client, inodes, config, read_cache, next_fh: 1, write_handles: HashMap::new(), notify_fd }
    }

    pub fn inodes(&self) -> Arc<Mutex<InodeTable>> { self.inodes.clone() }
    pub fn read_cache(&self) -> Arc<Mutex<ReadCache>> { self.read_cache.clone() }

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

    fn err_to_errno(e: &ClientError) -> i32 {
        match e {
            ClientError::NotFound => libc::ENOENT,
            ClientError::AlreadyExists => libc::EEXIST,
            ClientError::PermissionDenied => libc::EACCES,
            ClientError::Conflict => libc::EBUSY,
            ClientError::Io(_) => libc::EIO,
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

    fn cache_get(&self, path: &str) -> Option<Vec<u8>> {
        self.read_cache.lock().unwrap().get(path).map(|s| s.to_vec())
    }

    fn cache_put(&self, path: &str, data: Vec<u8>) {
        self.read_cache.lock().unwrap().put(path, data);
    }

    fn cache_invalidate(&self, path: &str) {
        self.read_cache.lock().unwrap().invalidate(path);
    }

    fn cache_is_cacheable(&self, size: u64) -> bool {
        self.read_cache.lock().unwrap().is_cacheable_size(size)
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

        match self.client.stat(&child_path) {
            Ok(info) => {
                let ino = self.inode_get_or_create(&child_path);
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
            if new_size == 0 {
                if let Err(ref e) = self.client.write_file(&path, b"", None) {
                    reply.error(Self::err_to_errno(e)); return;
                }
            } else {
                match self.client.read_file(&path) {
                    Ok(content) => {
                        let mut bytes = content.into_bytes();
                        let target = new_size as usize;
                        if target < bytes.len() { bytes.truncate(target); }
                        else if target > bytes.len() { bytes.resize(target, 0); }
                        if let Err(ref e) = self.client.write_file(&path, &bytes, None) {
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
        let entries = match self.client.list_dir(&path) {
            Ok(e) => e,
            Err(ref e) => { reply.error(Self::err_to_errno(e)); return; }
        };
        let parent_ino = self.inode_get_or_create(parent_path(&path));
        let mut full_entries = vec![
            (ino, FileType::Directory, ".".to_string()),
            (parent_ino, FileType::Directory, "..".to_string()),
        ];
        for de in &entries {
            let child_ino = self.inode_get_or_create(&de.path);
            let kind = if de.is_dir { FileType::Directory } else { FileType::RegularFile };
            full_entries.push((child_ino, kind, de.name.clone()));
        }
        for (i, (child_ino, kind, name)) in full_entries.iter().enumerate().skip(offset as usize) {
            if reply.add(*child_ino, (i + 1) as i64, *kind, name) { break; }
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
            let (existing, base_rev) = match self.client.stat(&path) {
                Ok(info) => {
                    let rev = info.revision;
                    if (flags & libc::O_TRUNC) != 0 {
                        (Vec::new(), rev)
                    } else {
                        let buf = match self.client.read_file(&path) {
                            Ok(content) => content.into_bytes(),
                            Err(ClientError::NotFound) => Vec::new(),
                            Err(ref e) => { reply.error(Self::err_to_errno(e)); return; }
                        };
                        (buf, rev)
                    }
                }
                Err(ClientError::NotFound) => (Vec::new(), None),
                Err(ref e) => { reply.error(Self::err_to_errno(e)); return; }
            };
            self.write_handles.insert(fh, WriteHandle { ino, buf: existing, dirty: false, base_rev });
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

        if let Some(cached) = self.cache_get(&path) {
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
                Ok(content) => {
                    let bytes = content.into_bytes();
                    let off = offset as usize;
                    if off >= bytes.len() { reply.data(&[]); }
                    else {
                        let end = std::cmp::min(off + size as usize, bytes.len());
                        reply.data(&bytes[off..end]);
                    }
                    self.cache_put(&path, bytes);
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
        let handle = self.write_handles.entry(fh).or_insert_with(|| WriteHandle {
            ino, buf: Vec::new(), dirty: false, base_rev: None,
        });
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
        let fh = self.alloc_fh();
        let ino = self.inode_get_or_create(&child_path);
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
        let ino = self.inode_get_or_create(&child_path);
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
            Ok(()) => { self.inode_remove(&child_path); reply.ok(); }
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
                self.inode_rename(&old_path, &new_path);
                self.cache_invalidate(&old_path);
                reply.ok();
            }
            Err(ref e) => reply.error(Self::err_to_errno(e)),
        }
    }

    fn opendir(&mut self, _req: &Request, _ino: u64, _flags: i32, reply: ReplyOpen) {
        reply.opened(0, 0);
    }

    fn statfs(&mut self, _req: &Request, _ino: u64, reply: fuser::ReplyStatfs) {
        reply.statfs(0, 0, 0, 0, 0, BLOCK_SIZE, 255, 0);
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
