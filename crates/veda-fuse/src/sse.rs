use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tracing::{debug, info, warn};

use crate::cache::ReadCache;
use crate::fs::{parent_path, DirCacheMap};
use crate::inode::InodeTable;

const RECONNECT_MIN: Duration = Duration::from_secs(1);
const RECONNECT_MAX: Duration = Duration::from_secs(30);
const CURSOR_FLUSH_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, serde::Deserialize)]
struct SseEvent {
    #[allow(dead_code)]
    id: i64,
    #[allow(dead_code)]
    event_type: String,
    path: String,
}

pub struct SseWatcher {
    stop: Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl SseWatcher {
    pub fn start(
        server: &str,
        key: &str,
        inodes: Arc<Mutex<InodeTable>>,
        read_cache: Arc<Mutex<ReadCache>>,
        dir_cache: DirCacheMap,
        cursor_file: PathBuf,
    ) -> Self {
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_clone = stop.clone();
        let server = server.to_string();
        let key = key.to_string();

        let handle = std::thread::spawn(move || {
            let http = reqwest::blocking::Client::builder()
                .timeout(None)
                .build()
                .expect("failed to build SSE client");

            let mut cursor: i64 = load_cursor(&cursor_file);
            let mut last_persisted = cursor;
            let mut last_flush = Instant::now();
            info!(cursor, "SSE starting with persisted cursor");
            let mut backoff = RECONNECT_MIN;

            while !stop_clone.load(std::sync::atomic::Ordering::Relaxed) {
                let url = format!(
                    "{}/v1/events?since_id={}",
                    server.trim_end_matches('/'),
                    cursor,
                );

                match http.get(&url).bearer_auth(&key).send() {
                    Ok(resp) if resp.status().is_success() => {
                        info!("SSE connected to {url}");
                        backoff = RECONNECT_MIN;

                        let reader = std::io::BufReader::new(resp);
                        for line in reader.lines() {
                            if stop_clone.load(std::sync::atomic::Ordering::Relaxed) {
                                flush_cursor_if_dirty(&cursor_file, cursor, &mut last_persisted);
                                return;
                            }
                            let line = match line {
                                Ok(l) => l,
                                Err(e) => {
                                    warn!("SSE read error: {e}");
                                    break;
                                }
                            };

                            if let Some(data) = line.strip_prefix("data:") {
                                let data = data.trim();
                                if data.is_empty() {
                                    continue;
                                }
                                if let Ok(event) = serde_json::from_str::<SseEvent>(data) {
                                    debug!(
                                        event_type = %event.event_type,
                                        path = %event.path,
                                        "SSE event received"
                                    );

                                    cursor = event.id;
                                    invalidate_caches(
                                        &event.path,
                                        &inodes,
                                        &read_cache,
                                        &dir_cache,
                                    );
                                }
                            }

                            if let Some(id_str) = line.strip_prefix("id:") {
                                if let Ok(id) = id_str.trim().parse::<i64>() {
                                    cursor = id;
                                }
                            }

                            if cursor != last_persisted && last_flush.elapsed() >= CURSOR_FLUSH_INTERVAL {
                                save_cursor(&cursor_file, cursor);
                                last_persisted = cursor;
                                last_flush = Instant::now();
                            }
                        }
                        flush_cursor_if_dirty(&cursor_file, cursor, &mut last_persisted);
                    }
                    Ok(resp) => {
                        warn!("SSE connect failed: HTTP {}", resp.status());
                    }
                    Err(e) => {
                        warn!("SSE connect error: {e}");
                    }
                }

                if stop_clone.load(std::sync::atomic::Ordering::Relaxed) {
                    flush_cursor_if_dirty(&cursor_file, cursor, &mut last_persisted);
                    return;
                }
                std::thread::sleep(backoff);
                backoff = std::cmp::min(backoff * 2, RECONNECT_MAX);
            }
            flush_cursor_if_dirty(&cursor_file, cursor, &mut last_persisted);
        });

        Self {
            stop,
            handle: Some(handle),
        }
    }

    pub fn stop(&mut self) {
        self.stop
            .store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for SseWatcher {
    fn drop(&mut self) {
        self.stop();
    }
}

fn flush_cursor_if_dirty(path: &Path, cursor: i64, last_persisted: &mut i64) {
    if cursor != *last_persisted {
        save_cursor(path, cursor);
        *last_persisted = cursor;
    }
}

fn load_cursor(path: &Path) -> i64 {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

/// Atomic write: write to .tmp then rename so a crash can't corrupt the file.
fn save_cursor(path: &Path, cursor: i64) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, cursor.to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

fn invalidate_caches(
    path: &str,
    inodes: &Arc<Mutex<InodeTable>>,
    read_cache: &Arc<Mutex<ReadCache>>,
    dir_cache: &DirCacheMap,
) {
    if let Ok(mut cache) = read_cache.lock() {
        cache.invalidate(path);
    }

    let parent = parent_path(path);
    let parent_ino = if let Ok(mut table) = inodes.lock() {
        if let Some(ino) = table.get_ino(path) {
            table.invalidate(ino);
        }
        let pi = table.get_ino(parent);
        if let Some(ino) = pi {
            table.invalidate(ino);
        }
        pi
    } else {
        None
    };
    if let Some(pi) = parent_ino {
        if let Ok(mut dc) = dir_cache.lock() {
            dc.remove(&pi);
        }
    }
}

pub fn cursor_file_path(server: &str) -> PathBuf {
    let safe = server
        .replace("://", "_")
        .replace(['/', ':', '?', '#', '@'], "_");
    let cache_dir = std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
        .join(".cache/veda-fuse");
    cache_dir.join(format!("cursor-{safe}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parent_path_root() {
        assert_eq!(parent_path("/"), "/");
        assert_eq!(parent_path("/a.txt"), "/");
        assert_eq!(parent_path("/docs/a.txt"), "/docs");
        assert_eq!(parent_path("/a/b/c"), "/a/b");
    }

    #[test]
    fn sse_event_deserialize() {
        let json = r#"{"id":42,"event_type":"update","path":"/docs/a.txt"}"#;
        let event: SseEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.id, 42);
        assert_eq!(event.event_type, "update");
        assert_eq!(event.path, "/docs/a.txt");
    }

    #[test]
    fn sse_event_extra_fields_ignored() {
        let json = r#"{"id":1,"event_type":"create","path":"/new.txt","file_id":"abc"}"#;
        let event: SseEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.path, "/new.txt");
    }

    use crate::fs::DirCacheMap;
    use std::collections::HashMap;

    #[test]
    fn invalidate_caches_removes_parent_dir_cache_entry() {
        let inodes = Arc::new(Mutex::new(InodeTable::new()));
        let read_cache = Arc::new(Mutex::new(ReadCache::new(1, Duration::from_secs(30))));
        let dir_cache: DirCacheMap = Arc::new(Mutex::new(HashMap::new()));

        let foo_ino = {
            let mut t = inodes.lock().unwrap();
            t.get_or_create_ino("/foo")
        };
        {
            let mut dc = dir_cache.lock().unwrap();
            dc.insert(foo_ino, crate::fs::DirCacheEntry::empty_for_test());
        }

        invalidate_caches("/foo/new.txt", &inodes, &read_cache, &dir_cache);

        assert!(dir_cache.lock().unwrap().get(&foo_ino).is_none());
    }

    #[test]
    fn invalidate_caches_still_invalidates_read_and_attr_caches() {
        let inodes = Arc::new(Mutex::new(InodeTable::new()));
        let read_cache = Arc::new(Mutex::new(ReadCache::new(1, Duration::from_secs(30))));
        let dir_cache: DirCacheMap = Arc::new(Mutex::new(HashMap::new()));

        let file_ino = {
            let mut t = inodes.lock().unwrap();
            let ino = t.get_or_create_ino("/foo/bar.txt");
            t.get_or_create_ino("/foo");
            let dummy_attr = fuser::FileAttr {
                ino, size: 100,
                blocks: 1,
                atime: std::time::SystemTime::UNIX_EPOCH,
                mtime: std::time::SystemTime::UNIX_EPOCH,
                ctime: std::time::SystemTime::UNIX_EPOCH,
                crtime: std::time::SystemTime::UNIX_EPOCH,
                kind: fuser::FileType::RegularFile, perm: 0o644, nlink: 1,
                uid: 0, gid: 0, rdev: 0, blksize: 512, flags: 0,
            };
            t.set_cached_attr(ino, dummy_attr);
            ino
        };
        {
            let gen = read_cache.lock().unwrap().generation();
            read_cache.lock().unwrap().put("/foo/bar.txt", b"hello".to_vec(), gen);
        }

        invalidate_caches("/foo/bar.txt", &inodes, &read_cache, &dir_cache);

        assert!(inodes.lock().unwrap().get_cached_attr(file_ino).is_none());
        assert!(read_cache.lock().unwrap().get("/foo/bar.txt").is_none());
    }

    #[test]
    fn cursor_persistence_roundtrip() {
        let dir = std::env::temp_dir().join("veda-fuse-test-cursor");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test-cursor");
        let _ = std::fs::remove_file(&path);

        assert_eq!(load_cursor(&path), 0);
        save_cursor(&path, 42);
        assert_eq!(load_cursor(&path), 42);
        save_cursor(&path, 99);
        assert_eq!(load_cursor(&path), 99);

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn cursor_file_path_stable() {
        let p1 = cursor_file_path("https://example.com");
        let p2 = cursor_file_path("https://example.com");
        assert_eq!(p1, p2);
        let p3 = cursor_file_path("https://other.com");
        assert_ne!(p1, p3);
    }

    #[test]
    fn cursor_file_path_no_special_chars() {
        let p = cursor_file_path("https://api.example.com:8443/prefix");
        let name = p.file_name().unwrap().to_str().unwrap();
        assert!(!name.contains('/'));
        assert!(!name.contains(':'));
    }
}
