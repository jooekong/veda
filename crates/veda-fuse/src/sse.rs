use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tracing::{debug, info, warn};

use crate::cache::ReadCache;
use crate::fs::{parent_path, DirCacheMap, SidecarMissCache, MAGIC_NAMES};
use crate::inode::InodeTable;

const RECONNECT_MIN: Duration = Duration::from_secs(1);
const RECONNECT_MAX: Duration = Duration::from_secs(30);
const CURSOR_FLUSH_INTERVAL: Duration = Duration::from_secs(1);
// Cap per-connection lifetime so a TCP-half-open or silently-stalled SSE
// connection can't keep us blocked forever. After this we error out, the
// outer loop reconnects with `since_id=cursor`, and we resume mid-stream.
const SSE_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, serde::Deserialize)]
struct SseEvent {
    #[allow(dead_code)]
    id: i64,
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
        sidecar_miss: SidecarMissCache,
        cursor_file: PathBuf,
    ) -> Self {
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_clone = stop.clone();
        let server = server.to_string();
        let key = key.to_string();

        let handle = std::thread::spawn(move || {
            let http = reqwest::blocking::Client::builder()
                .timeout(SSE_REQUEST_TIMEOUT)
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
                    Ok(resp) if resp.status().as_u16() == 410 => {
                        // Cursor fell off the server's retention window. The
                        // body carries `current_max_id` (or `current_min_id`
                        // when the table is empty) — both are post-cutoff
                        // floors we can safely jump our cursor to. We then
                        // wipe attr/read/dir caches because file structure
                        // may have changed entirely while we slept; the
                        // kernel still owns inode refs so we don't touch
                        // path↔ino mappings (kernel re-fetches on next access
                        // via readdir / getattr after the cache wipe — there's
                        // no eager list_dir resync here on purpose).
                        let new_cursor = parse_410_cursor(resp);
                        warn!(
                            old = cursor,
                            new = new_cursor,
                            "SSE 410: cursor expired, resyncing"
                        );
                        handle_cursor_expired(&inodes, &read_cache, &dir_cache, &sidecar_miss);
                        cursor = new_cursor;
                        // Only mark in-memory persisted state when the on-disk
                        // write actually succeeded — otherwise a process crash
                        // here could roll us back to the old (pre-410) cursor
                        // even though we've already advanced in memory and
                        // wiped caches.
                        if save_cursor(&cursor_file, cursor) {
                            last_persisted = cursor;
                            last_flush = Instant::now();
                        }
                        // Exponential backoff inside the 410 branch only.
                        // A misconfigured server (retention=0) or a broken
                        // resync would otherwise pin us to a 1Hz hot loop.
                        // The success branch resets back to MIN once a real
                        // connection lands, so a transient 410 burst recovers
                        // quickly; only chronic 410s pay the backoff cost.
                        backoff = std::cmp::min(backoff.saturating_mul(2), RECONNECT_MAX);
                    }
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
                                        &event.event_type,
                                        &event.path,
                                        &inodes,
                                        &read_cache,
                                        &dir_cache,
                                        &sidecar_miss,
                                    );
                                }
                            }

                            if let Some(id_str) = line.strip_prefix("id:") {
                                if let Ok(id) = id_str.trim().parse::<i64>() {
                                    cursor = id;
                                }
                            }

                            if cursor != last_persisted && last_flush.elapsed() >= CURSOR_FLUSH_INTERVAL {
                                if save_cursor(&cursor_file, cursor) {
                                    last_persisted = cursor;
                                    last_flush = Instant::now();
                                }
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
        // Only mark persisted on a successful disk write — otherwise we'd
        // suppress retry on the next dirty check and silently keep diverging
        // from the on-disk truth.
        if save_cursor(path, cursor) {
            *last_persisted = cursor;
        }
    }
}

fn load_cursor(path: &Path) -> i64 {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

/// Atomic write: write to .tmp then rename so a crash can't corrupt the file.
/// Returns true on a successful rename, false otherwise so the caller can
/// avoid lying about persisted state.
fn save_cursor(path: &Path, cursor: i64) -> bool {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, cursor.to_string()).is_err() {
        return false;
    }
    std::fs::rename(&tmp, path).is_ok()
}

/// Parse the 410 body to extract the cursor we should resume from.
/// The contract (server `routes/events.rs::build_410_response`) is:
///   { current_min_id: i64, current_max_id: i64 | null }
/// Prefer `current_max_id`: jumping straight to the head means the next
/// stream resumes with id > max, so we won't replay events we'll re-discover
/// via the post-410 list_dir resync. Fall back to `current_min_id` when the
/// table is empty (max is null) — that just means "subscribe fresh from
/// the new floor". Final fallback to 0 if the body is missing/malformed —
/// worst case we replay events but never silently lose them.
fn parse_410_cursor(resp: reqwest::blocking::Response) -> i64 {
    // Read text first so a non-JSON 410 body (e.g. an HTML error page from a
    // reverse proxy) shows up in the warn log instead of being lost as
    // "expected value at line 1 column 1".
    let body = resp.text().unwrap_or_default();
    match serde_json::from_str::<serde_json::Value>(&body) {
        Ok(v) => parse_410_cursor_from_json(&v),
        Err(e) => {
            let snippet: String = body.chars().take(200).collect();
            warn!(
                err = %e,
                body = %snippet,
                "SSE 410: failed to parse body, falling back to cursor=0"
            );
            0
        }
    }
}

fn parse_410_cursor_from_json(v: &serde_json::Value) -> i64 {
    if let Some(n) = v.get("current_max_id").and_then(|x| x.as_i64()) {
        return n;
    }
    if let Some(n) = v.get("current_min_id").and_then(|x| x.as_i64()) {
        return n;
    }
    0
}

/// Caches to drop on cursor-expired (410). Symmetrical to the per-event
/// path in `invalidate_caches` but workspace-wide: we don't know which
/// paths changed during the gap, so the safe default is "trust nothing".
/// path↔ino mappings stay intact — the kernel owns those refs and forget
/// is the only safe way to drop them.
fn handle_cursor_expired(
    inodes: &Arc<Mutex<InodeTable>>,
    read_cache: &Arc<Mutex<ReadCache>>,
    dir_cache: &DirCacheMap,
    sidecar_miss: &SidecarMissCache,
) {
    if let Ok(mut t) = inodes.lock() {
        t.invalidate_all_attrs();
    }
    if let Ok(mut c) = read_cache.lock() {
        c.invalidate_all();
    }
    if let Ok(mut d) = dir_cache.lock() {
        d.clear();
    }
    if let Ok(mut s) = sidecar_miss.lock() {
        s.clear();
    }
}

fn invalidate_caches(
    event_type: &str,
    path: &str,
    inodes: &Arc<Mutex<InodeTable>>,
    read_cache: &Arc<Mutex<ReadCache>>,
    dir_cache: &DirCacheMap,
    sidecar_miss: &SidecarMissCache,
) {
    // SummaryReady: the sidecar listing at `path` is now valid. Three
    // pieces of stale state may exist for the magic sidecars under
    // this dir; each must drop so the next read/stat re-probes the
    // server instead of serving cached placeholder data:
    //   1. per-dir miss cache (the obvious one — caller never saw a
    //      sidecar because we cached the ENOENT);
    //   2. read cache for the sidecar path itself (caller had hit the
    //      pending sidecar earlier and got the `SIDECAR_PENDING_BODY`
    //      placeholder bytes cached);
    //   3. synthetic attr for that path (size attribute reflected the
    //      placeholder body, not the real summary).
    // dir_cache stays untouched — the directory's real contents didn't
    // change, only the synthetic sidecar entries inside it did.
    if event_type == "summary_ready" {
        if let Ok(mut guard) = sidecar_miss.lock() {
            for (magic, _) in MAGIC_NAMES {
                guard.remove(&(path.to_string(), *magic));
            }
        }
        for (magic, _) in MAGIC_NAMES {
            let sidecar_path = if path == "/" {
                format!("/{magic}")
            } else {
                format!("{path}/{magic}")
            };
            if let Ok(mut cache) = read_cache.lock() {
                cache.invalidate(&sidecar_path);
            }
            if let Ok(mut table) = inodes.lock() {
                if let Some(ino) = table.get_ino(&sidecar_path) {
                    table.invalidate(ino);
                }
            }
        }
        return;
    }

    if let Ok(mut cache) = read_cache.lock() {
        cache.invalidate(path);
    }

    // Delete events must drop the path → ino mapping entirely. Otherwise a
    // subsequent `lookup` for a re-created path with the same name would hit
    // the stale ino, the kernel would think the file is the same one it had
    // open, and the user would see unexpected reads against the deleted file's
    // (now reused) state. Update events only need attr-cache invalidation —
    // the file at this path still exists, just with new content.
    let is_delete = event_type == "delete";
    let parent = parent_path(path);
    let parent_ino = if let Ok(mut table) = inodes.lock() {
        if is_delete {
            table.remove_path(path);
        } else if let Some(ino) = table.get_ino(path) {
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

    fn empty_sidecar_miss() -> SidecarMissCache {
        Arc::new(Mutex::new(HashMap::new()))
    }

    #[test]
    fn invalidate_caches_removes_parent_dir_cache_entry() {
        let inodes = Arc::new(Mutex::new(InodeTable::new()));
        let read_cache = Arc::new(Mutex::new(ReadCache::new(1, Duration::from_secs(30))));
        let dir_cache: DirCacheMap = Arc::new(Mutex::new(HashMap::new()));
        let sm = empty_sidecar_miss();

        let foo_ino = {
            let mut t = inodes.lock().unwrap();
            t.get_or_create_ino("/foo")
        };
        {
            let mut dc = dir_cache.lock().unwrap();
            dc.insert(foo_ino, crate::fs::DirCacheEntry::empty_for_test());
        }

        invalidate_caches("update", "/foo/new.txt", &inodes, &read_cache, &dir_cache, &sm);

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

        invalidate_caches("update", "/foo/bar.txt", &inodes, &read_cache, &dir_cache, &empty_sidecar_miss());

        assert!(inodes.lock().unwrap().get_cached_attr(file_ino).is_none());
        assert!(read_cache.lock().unwrap().get("/foo/bar.txt").is_none());
    }

    #[test]
    fn invalidate_caches_delete_drops_path_to_ino_mapping() {
        let inodes = Arc::new(Mutex::new(InodeTable::new()));
        let read_cache = Arc::new(Mutex::new(ReadCache::new(1, Duration::from_secs(30))));
        let dir_cache: DirCacheMap = Arc::new(Mutex::new(HashMap::new()));

        let file_path = "/foo/bar.txt";
        let file_ino = {
            let mut t = inodes.lock().unwrap();
            t.get_or_create_ino(file_path)
        };

        invalidate_caches("delete", file_path, &inodes, &read_cache, &dir_cache, &empty_sidecar_miss());

        let table = inodes.lock().unwrap();
        assert!(
            table.get_ino(file_path).is_none(),
            "delete must drop path→ino mapping"
        );
        assert!(
            table.get_path(file_ino).is_none(),
            "delete must drop ino→path mapping"
        );
    }

    #[test]
    fn parse_410_cursor_prefers_max_over_min() {
        let v = serde_json::json!({"current_min_id": 100, "current_max_id": 999});
        assert_eq!(parse_410_cursor_from_json(&v), 999);
    }

    #[test]
    fn parse_410_cursor_falls_back_to_min_when_max_null() {
        let v = serde_json::json!({"current_min_id": 42, "current_max_id": null});
        assert_eq!(parse_410_cursor_from_json(&v), 42);
    }

    #[test]
    fn parse_410_cursor_falls_back_to_zero_when_body_malformed() {
        let v = serde_json::json!({"unrelated": "junk"});
        assert_eq!(parse_410_cursor_from_json(&v), 0);
    }

    #[test]
    fn handle_cursor_expired_clears_all_three_caches() {
        let inodes = Arc::new(Mutex::new(InodeTable::new()));
        let read_cache = Arc::new(Mutex::new(ReadCache::new(2, Duration::from_secs(30))));
        let dir_cache: DirCacheMap = Arc::new(Mutex::new(HashMap::new()));

        // Seed all three caches with stuff that should be wiped.
        let (foo_ino, bar_ino) = {
            let mut t = inodes.lock().unwrap();
            let foo = t.get_or_create_ino("/foo");
            let bar = t.get_or_create_ino("/foo/bar.txt");
            let dummy_attr = fuser::FileAttr {
                ino: bar, size: 100,
                blocks: 1,
                atime: std::time::SystemTime::UNIX_EPOCH,
                mtime: std::time::SystemTime::UNIX_EPOCH,
                ctime: std::time::SystemTime::UNIX_EPOCH,
                crtime: std::time::SystemTime::UNIX_EPOCH,
                kind: fuser::FileType::RegularFile, perm: 0o644, nlink: 1,
                uid: 0, gid: 0, rdev: 0, blksize: 512, flags: 0,
            };
            t.set_cached_attr(bar, dummy_attr);
            (foo, bar)
        };
        {
            let gen = read_cache.lock().unwrap().generation();
            read_cache.lock().unwrap().put("/foo/bar.txt", b"v1".to_vec(), gen);
        }
        {
            let mut dc = dir_cache.lock().unwrap();
            dc.insert(foo_ino, crate::fs::DirCacheEntry::empty_for_test());
        }

        let sm = empty_sidecar_miss();
        sm.lock().unwrap().insert(("/foo".to_string(), ".abstract"), Instant::now());
        handle_cursor_expired(&inodes, &read_cache, &dir_cache, &sm);

        assert!(
            sm.lock().unwrap().is_empty(),
            "cursor-expired must drop sidecar_miss too — we don't know which dirs gained sidecars during the gap"
        );

        // Attr cache wiped, but path↔ino mappings preserved (kernel still
        // holds inode refs and would explode if we yanked them).
        let t = inodes.lock().unwrap();
        assert!(t.get_cached_attr(bar_ino).is_none());
        assert_eq!(t.get_ino("/foo"), Some(foo_ino));
        assert_eq!(t.get_ino("/foo/bar.txt"), Some(bar_ino));
        drop(t);

        assert!(read_cache.lock().unwrap().get("/foo/bar.txt").is_none());
        assert!(dir_cache.lock().unwrap().is_empty());
    }

    #[test]
    fn invalidate_caches_update_keeps_path_to_ino_mapping() {
        let inodes = Arc::new(Mutex::new(InodeTable::new()));
        let read_cache = Arc::new(Mutex::new(ReadCache::new(1, Duration::from_secs(30))));
        let dir_cache: DirCacheMap = Arc::new(Mutex::new(HashMap::new()));

        let file_path = "/foo/bar.txt";
        let file_ino = {
            let mut t = inodes.lock().unwrap();
            t.get_or_create_ino(file_path)
        };

        invalidate_caches("update", file_path, &inodes, &read_cache, &dir_cache, &empty_sidecar_miss());

        let table = inodes.lock().unwrap();
        assert_eq!(table.get_ino(file_path), Some(file_ino));
    }

    #[test]
    fn invalidate_caches_summary_ready_drops_sidecar_state_only() {
        // SummaryReady fires after the dir-summary worker upserts a row.
        // Three pieces of state must drop for both magic sidecars under
        // the event dir:
        //   1. miss cache (so the next lookup probes the server)
        //   2. read cache (so a cached `SIDECAR_PENDING_BODY` doesn't
        //      keep being served as if it were the real summary)
        //   3. synthetic attr in the inode table
        // Everything else — unrelated dirs, non-sidecar read entries,
        // dir_cache — must stay untouched.
        let inodes = Arc::new(Mutex::new(InodeTable::new()));
        let read_cache = Arc::new(Mutex::new(ReadCache::new(1, Duration::from_secs(30))));
        let dir_cache: DirCacheMap = Arc::new(Mutex::new(HashMap::new()));
        let sm = empty_sidecar_miss();

        // Seed the miss cache for /docs and an unrelated /notes path. Only
        // /docs entries should disappear after the event fires; /notes is
        // the canary that confirms we don't clear the whole map.
        let now = Instant::now();
        {
            let mut g = sm.lock().unwrap();
            g.insert(("/docs".to_string(), ".abstract"), now);
            g.insert(("/docs".to_string(), ".overview"), now);
            g.insert(("/notes".to_string(), ".abstract"), now);
        }

        // Seed inode attr + read_cache for the sidecar paths (the
        // "pending placeholder" state we want flushed) and for an
        // unrelated regular file (the canary that proves we scope
        // invalidation to the magic sidecars only).
        let (dir_ino, abstract_ino, overview_ino) = {
            let mut t = inodes.lock().unwrap();
            let dir = t.get_or_create_ino("/docs");
            let a = t.get_or_create_ino("/docs/.abstract");
            let o = t.get_or_create_ino("/docs/.overview");
            let dummy_attr = |ino| fuser::FileAttr {
                ino, size: 44, blocks: 1,
                atime: std::time::SystemTime::UNIX_EPOCH,
                mtime: std::time::SystemTime::UNIX_EPOCH,
                ctime: std::time::SystemTime::UNIX_EPOCH,
                crtime: std::time::SystemTime::UNIX_EPOCH,
                kind: fuser::FileType::RegularFile, perm: 0o644, nlink: 1,
                uid: 0, gid: 0, rdev: 0, blksize: 512, flags: 0,
            };
            t.set_cached_attr(a, dummy_attr(a));
            t.set_cached_attr(o, dummy_attr(o));
            (dir, a, o)
        };
        {
            let mut dc = dir_cache.lock().unwrap();
            dc.insert(dir_ino, crate::fs::DirCacheEntry::empty_for_test());
        }
        {
            let mut c = read_cache.lock().unwrap();
            let gen = c.generation();
            c.put("/docs/.abstract", b"pending\n".to_vec(), gen);
            c.put("/docs/.overview", b"pending\n".to_vec(), gen);
            c.put("/docs/file.md", b"x".to_vec(), gen);
        }

        invalidate_caches("summary_ready", "/docs", &inodes, &read_cache, &dir_cache, &sm);

        // 1. miss cache: scoped to /docs.
        let g = sm.lock().unwrap();
        assert!(g.get(&("/docs".to_string(), ".abstract")).is_none());
        assert!(g.get(&("/docs".to_string(), ".overview")).is_none());
        assert!(
            g.get(&("/notes".to_string(), ".abstract")).is_some(),
            "summary_ready must scope to the event path; unrelated dirs untouched"
        );
        drop(g);

        // 2. read cache: only sidecar paths flushed.
        let mut c = read_cache.lock().unwrap();
        assert!(c.get("/docs/.abstract").is_none(), "stale .abstract body must drop");
        assert!(c.get("/docs/.overview").is_none(), "stale .overview body must drop");
        assert!(
            c.get("/docs/file.md").is_some(),
            "non-sidecar read entries must NOT be invalidated"
        );
        drop(c);

        // 3. inode attr: only sidecar attrs flushed.
        let t = inodes.lock().unwrap();
        assert!(t.get_cached_attr(abstract_ino).is_none());
        assert!(t.get_cached_attr(overview_ino).is_none());
        drop(t);

        // 4. dir_cache untouched — the directory itself didn't change.
        assert!(
            dir_cache.lock().unwrap().get(&dir_ino).is_some(),
            "dir_cache must not be invalidated on summary_ready"
        );
    }

    #[test]
    fn cursor_persistence_roundtrip() {
        let dir = std::env::temp_dir().join("veda-fuse-test-cursor");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test-cursor");
        let _ = std::fs::remove_file(&path);

        assert_eq!(load_cursor(&path), 0);
        assert!(save_cursor(&path, 42));
        assert_eq!(load_cursor(&path), 42);
        assert!(save_cursor(&path, 99));
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
