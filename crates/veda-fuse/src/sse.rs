use std::io::BufRead;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tracing::{debug, info, warn};

use crate::cache::ReadCache;
use crate::fs::parent_path;
use crate::inode::InodeTable;

const RECONNECT_MIN: Duration = Duration::from_secs(1);
const RECONNECT_MAX: Duration = Duration::from_secs(30);

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

            let mut cursor: i64 = 0;
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
                                    );
                                }
                            }

                            if let Some(id_str) = line.strip_prefix("id:") {
                                if let Ok(id) = id_str.trim().parse::<i64>() {
                                    cursor = id;
                                }
                            }
                        }
                    }
                    Ok(resp) => {
                        warn!("SSE connect failed: HTTP {}", resp.status());
                    }
                    Err(e) => {
                        warn!("SSE connect error: {e}");
                    }
                }

                if stop_clone.load(std::sync::atomic::Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(backoff);
                backoff = std::cmp::min(backoff * 2, RECONNECT_MAX);
            }
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

fn invalidate_caches(
    path: &str,
    inodes: &Arc<Mutex<InodeTable>>,
    read_cache: &Arc<Mutex<ReadCache>>,
) {
    // Invalidate read cache for the file
    if let Ok(mut cache) = read_cache.lock() {
        cache.invalidate(path);
    }

    // Invalidate inode attr cache for the file and its parent directory
    if let Ok(mut table) = inodes.lock() {
        if let Some(ino) = table.get_ino(path) {
            table.invalidate(ino);
        }
        let parent = parent_path(path);
        if let Some(parent_ino) = table.get_ino(parent) {
            table.invalidate(parent_ino);
        }
    }
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
}
