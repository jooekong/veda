use std::collections::HashMap;
use std::ffi::OsStr;
use std::time::{Duration, SystemTime};

use fuser::{
    FileAttr, FileType, Filesystem, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty,
    ReplyEntry, ReplyOpen, ReplyWrite, Request,
};
use tracing::{debug, warn};

use crate::client::{ClientError, FileInfo, VedaClient};
use crate::inode::InodeTable;

const TTL: Duration = Duration::from_secs(5);
const BLOCK_SIZE: u32 = 512;

pub struct VedaFs {
    client: VedaClient,
    inodes: InodeTable,
    next_fh: u64,
    write_buffers: HashMap<u64, (u64, Vec<u8>)>, // fh -> (ino, data)
}

impl VedaFs {
    pub fn new(client: VedaClient) -> Self {
        Self {
            client,
            inodes: InodeTable::new(),
            next_fh: 1,
            write_buffers: HashMap::new(),
        }
    }

    fn alloc_fh(&mut self) -> u64 {
        let fh = self.next_fh;
        self.next_fh += 1;
        fh
    }

    fn parent_path(path: &str) -> &str {
        if path == "/" { return "/"; }
        match path.rfind('/') {
            Some(0) => "/",
            Some(i) => &path[..i],
            None => "/",
        }
    }

    fn info_to_attr(&mut self, info: &FileInfo, ino: u64) -> FileAttr {
        let kind = if info.is_dir { FileType::Directory } else { FileType::RegularFile };
        let size = info.size_bytes.unwrap_or(0) as u64;
        let perm = if info.is_dir { 0o755 } else { 0o644 };
        let now = SystemTime::now();

        let attr = FileAttr {
            ino,
            size,
            blocks: (size + BLOCK_SIZE as u64 - 1) / BLOCK_SIZE as u64,
            atime: now,
            mtime: now,
            ctime: now,
            crtime: now,
            kind,
            perm,
            nlink: if info.is_dir { 2 } else { 1 },
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            rdev: 0,
            blksize: BLOCK_SIZE,
            flags: 0,
        };
        self.inodes.set_cached_attr(ino, attr);
        attr
    }

    fn resolve_child_path(parent_path: &str, name: &str) -> String {
        if parent_path == "/" {
            format!("/{name}")
        } else {
            format!("{parent_path}/{name}")
        }
    }

    fn err_to_errno(e: &ClientError) -> i32 {
        match e {
            ClientError::NotFound => libc::ENOENT,
            ClientError::AlreadyExists => libc::EEXIST,
            ClientError::PermissionDenied => libc::EACCES,
            ClientError::Io(_) => libc::EIO,
        }
    }
}

impl Filesystem for VedaFs {
    fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let name_str = match name.to_str() {
            Some(s) => s,
            None => { reply.error(libc::ENOENT); return; }
        };

        let parent_path = match self.inodes.get_path(parent) {
            Some(p) => p.to_string(),
            None => { reply.error(libc::ENOENT); return; }
        };

        let child_path = Self::resolve_child_path(&parent_path, name_str);

        match self.client.stat(&child_path) {
            Ok(info) => {
                let ino = self.inodes.get_or_create_ino(&child_path);
                let attr = self.info_to_attr(&info, ino);
                reply.entry(&TTL, &attr, 0);
            }
            Err(ref e) => {
                debug!(path = %child_path, err = %e, "lookup miss");
                reply.error(Self::err_to_errno(e));
            }
        }
    }

    fn getattr(&mut self, _req: &Request, ino: u64, reply: ReplyAttr) {
        if let Some(attr) = self.inodes.get_cached_attr(ino) {
            reply.attr(&TTL, &attr);
            return;
        }

        let path = match self.inodes.get_path(ino) {
            Some(p) => p.to_string(),
            None => { reply.error(libc::ENOENT); return; }
        };

        match self.client.stat(&path) {
            Ok(info) => {
                let attr = self.info_to_attr(&info, ino);
                reply.attr(&TTL, &attr);
            }
            Err(ref e) => reply.error(Self::err_to_errno(e)),
        }
    }

    fn setattr(
        &mut self,
        _req: &Request,
        ino: u64,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<fuser::TimeOrNow>,
        _mtime: Option<fuser::TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        if let Some(new_size) = size {
            let path = match self.inodes.get_path(ino) {
                Some(p) => p.to_string(),
                None => { reply.error(libc::ENOENT); return; }
            };

            if new_size == 0 {
                if let Err(ref e) = self.client.write_file(&path, b"") {
                    reply.error(Self::err_to_errno(e));
                    return;
                }
            } else {
                match self.client.read_file(&path) {
                    Ok(content) => {
                        let mut bytes = content.into_bytes();
                        let target = new_size as usize;
                        if target < bytes.len() {
                            bytes.truncate(target);
                        } else if target > bytes.len() {
                            bytes.resize(target, 0);
                        }
                        if let Err(ref e) = self.client.write_file(&path, &bytes) {
                            reply.error(Self::err_to_errno(e));
                            return;
                        }
                    }
                    Err(ref e) => { reply.error(Self::err_to_errno(e)); return; }
                }
            }
            self.inodes.invalidate(ino);
        }

        // Re-fetch attr
        let path = match self.inodes.get_path(ino) {
            Some(p) => p.to_string(),
            None => { reply.error(libc::ENOENT); return; }
        };
        match self.client.stat(&path) {
            Ok(info) => {
                let attr = self.info_to_attr(&info, ino);
                reply.attr(&TTL, &attr);
            }
            Err(ref e) => reply.error(Self::err_to_errno(e)),
        }
    }

    fn readdir(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let path = match self.inodes.get_path(ino) {
            Some(p) => p.to_string(),
            None => { reply.error(libc::ENOENT); return; }
        };

        let entries = match self.client.list_dir(&path) {
            Ok(e) => e,
            Err(ref e) => { reply.error(Self::err_to_errno(e)); return; }
        };

        let parent_ino = {
            let pp = Self::parent_path(&path);
            self.inodes.get_or_create_ino(pp)
        };

        let mut full_entries = vec![
            (ino, FileType::Directory, ".".to_string()),
            (parent_ino, FileType::Directory, "..".to_string()),
        ];

        for de in &entries {
            let child_ino = self.inodes.get_or_create_ino(&de.path);
            let kind = if de.is_dir { FileType::Directory } else { FileType::RegularFile };
            full_entries.push((child_ino, kind, de.name.clone()));
        }

        for (i, (child_ino, kind, name)) in full_entries.iter().enumerate().skip(offset as usize) {
            if reply.add(*child_ino, (i + 1) as i64, *kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn open(&mut self, _req: &Request, ino: u64, flags: i32, reply: ReplyOpen) {
        let fh = self.alloc_fh();
        let writable = (flags & libc::O_ACCMODE) != libc::O_RDONLY;

        if writable {
            let path = match self.inodes.get_path(ino) {
                Some(p) => p.to_string(),
                None => { reply.error(libc::ENOENT); return; }
            };

            let existing = if (flags & libc::O_TRUNC) != 0 {
                Vec::new()
            } else {
                match self.client.read_file(&path) {
                    Ok(content) => content.into_bytes(),
                    Err(ClientError::NotFound) => Vec::new(),
                    Err(ref e) => { reply.error(Self::err_to_errno(e)); return; }
                }
            };
            self.write_buffers.insert(fh, (ino, existing));
        }

        reply.opened(fh, 0);
    }

    fn read(
        &mut self,
        _req: &Request,
        ino: u64,
        fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        if let Some((_, buf)) = self.write_buffers.get(&fh) {
            let offset = offset as usize;
            if offset >= buf.len() {
                reply.data(&[]);
            } else {
                let end = std::cmp::min(offset + size as usize, buf.len());
                reply.data(&buf[offset..end]);
            }
            return;
        }

        let path = match self.inodes.get_path(ino) {
            Some(p) => p.to_string(),
            None => { reply.error(libc::ENOENT); return; }
        };

        match self.client.read_file(&path) {
            Ok(content) => {
                let bytes = content.as_bytes();
                let offset = offset as usize;
                if offset >= bytes.len() {
                    reply.data(&[]);
                } else {
                    let end = std::cmp::min(offset + size as usize, bytes.len());
                    reply.data(&bytes[offset..end]);
                }
            }
            Err(ref e) => reply.error(Self::err_to_errno(e)),
        }
    }

    fn write(
        &mut self,
        _req: &Request,
        ino: u64,
        fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        let (_, buf) = self.write_buffers.entry(fh).or_insert_with(|| (ino, Vec::new()));
        let offset = offset as usize;

        if offset > buf.len() {
            buf.resize(offset, 0);
        }

        let end = offset + data.len();
        if end > buf.len() {
            buf.resize(end, 0);
        }
        buf[offset..end].copy_from_slice(data);

        reply.written(data.len() as u32);
    }

    fn flush(&mut self, _req: &Request, ino: u64, fh: u64, _lock_owner: u64, reply: ReplyEmpty) {
        if let Some((_, buf)) = self.write_buffers.get(&fh) {
            let path = match self.inodes.get_path(ino) {
                Some(p) => p.to_string(),
                None => { reply.error(libc::ENOENT); return; }
            };

            if let Err(ref e) = self.client.write_file(&path, buf) {
                warn!(path = %path, err = %e, "flush failed");
                reply.error(Self::err_to_errno(e));
                return;
            }
            self.inodes.invalidate(ino);
        }
        reply.ok();
    }

    fn release(
        &mut self,
        _req: &Request,
        ino: u64,
        fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        if let Some((_, buf)) = self.write_buffers.remove(&fh) {
            let path = match self.inodes.get_path(ino) {
                Some(p) => p.to_string(),
                None => { reply.error(libc::ENOENT); return; }
            };

            if let Err(ref e) = self.client.write_file(&path, &buf) {
                warn!(path = %path, err = %e, "release write failed");
                reply.error(Self::err_to_errno(e));
                return;
            }
            self.inodes.invalidate(ino);
        }
        reply.ok();
    }

    fn create(
        &mut self,
        _req: &Request,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let name_str = match name.to_str() {
            Some(s) => s,
            None => { reply.error(libc::EINVAL); return; }
        };

        let parent_path = match self.inodes.get_path(parent) {
            Some(p) => p.to_string(),
            None => { reply.error(libc::ENOENT); return; }
        };

        let child_path = Self::resolve_child_path(&parent_path, name_str);

        if let Err(ref e) = self.client.write_file(&child_path, b"") {
            reply.error(Self::err_to_errno(e));
            return;
        }

        let fh = self.alloc_fh();
        let ino = self.inodes.get_or_create_ino(&child_path);
        let info = FileInfo { path: child_path, is_dir: false, size_bytes: Some(0) };
        let attr = self.info_to_attr(&info, ino);
        self.write_buffers.insert(fh, (ino, Vec::new()));

        reply.created(&TTL, &attr, 0, fh, 0);
    }

    fn mkdir(
        &mut self,
        _req: &Request,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let name_str = match name.to_str() {
            Some(s) => s,
            None => { reply.error(libc::EINVAL); return; }
        };

        let parent_path = match self.inodes.get_path(parent) {
            Some(p) => p.to_string(),
            None => { reply.error(libc::ENOENT); return; }
        };

        let child_path = Self::resolve_child_path(&parent_path, name_str);

        if let Err(ref e) = self.client.mkdir(&child_path) {
            reply.error(Self::err_to_errno(e));
            return;
        }

        let ino = self.inodes.get_or_create_ino(&child_path);
        let info = FileInfo { path: child_path, is_dir: true, size_bytes: None };
        let attr = self.info_to_attr(&info, ino);
        reply.entry(&TTL, &attr, 0);
    }

    fn unlink(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let name_str = match name.to_str() {
            Some(s) => s,
            None => { reply.error(libc::EINVAL); return; }
        };

        let parent_path = match self.inodes.get_path(parent) {
            Some(p) => p.to_string(),
            None => { reply.error(libc::ENOENT); return; }
        };

        let child_path = Self::resolve_child_path(&parent_path, name_str);

        match self.client.delete(&child_path) {
            Ok(()) => {
                self.inodes.remove_path(&child_path);
                reply.ok();
            }
            Err(ref e) => reply.error(Self::err_to_errno(e)),
        }
    }

    fn rmdir(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let name_str = match name.to_str() {
            Some(s) => s,
            None => { reply.error(libc::EINVAL); return; }
        };

        let parent_path = match self.inodes.get_path(parent) {
            Some(p) => p.to_string(),
            None => { reply.error(libc::ENOENT); return; }
        };

        let child_path = Self::resolve_child_path(&parent_path, name_str);

        match self.client.list_dir(&child_path) {
            Ok(entries) if !entries.is_empty() => {
                reply.error(libc::ENOTEMPTY);
                return;
            }
            Err(ref e) => {
                reply.error(Self::err_to_errno(e));
                return;
            }
            _ => {}
        }

        match self.client.delete(&child_path) {
            Ok(()) => {
                self.inodes.remove_path(&child_path);
                reply.ok();
            }
            Err(ref e) => reply.error(Self::err_to_errno(e)),
        }
    }

    fn rename(
        &mut self,
        _req: &Request,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        _flags: u32,
        reply: ReplyEmpty,
    ) {
        let name_str = match name.to_str() {
            Some(s) => s,
            None => { reply.error(libc::EINVAL); return; }
        };
        let newname_str = match newname.to_str() {
            Some(s) => s,
            None => { reply.error(libc::EINVAL); return; }
        };

        let parent_path = match self.inodes.get_path(parent) {
            Some(p) => p.to_string(),
            None => { reply.error(libc::ENOENT); return; }
        };
        let newparent_path = match self.inodes.get_path(newparent) {
            Some(p) => p.to_string(),
            None => { reply.error(libc::ENOENT); return; }
        };

        let old_path = Self::resolve_child_path(&parent_path, name_str);
        let new_path = Self::resolve_child_path(&newparent_path, newname_str);

        match self.client.rename(&old_path, &new_path) {
            Ok(()) => {
                self.inodes.rename_path(&old_path, &new_path);
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

    fn init(
        &mut self,
        _req: &Request<'_>,
        _config: &mut fuser::KernelConfig,
    ) -> std::result::Result<(), libc::c_int> {
        debug!("FUSE init");
        Ok(())
    }

    fn destroy(&mut self) {
        debug!("FUSE destroy");
        for (_fh, (ino, buf)) in self.write_buffers.drain() {
            if !buf.is_empty() {
                if let Some(path) = self.inodes.get_path(ino) {
                    let _ = self.client.write_file(path, &buf);
                }
            }
        }
    }
}
