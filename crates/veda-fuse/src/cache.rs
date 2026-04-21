use std::collections::HashMap;
use std::time::{Duration, Instant};

const DEFAULT_MAX_ENTRY_SIZE: usize = 1024 * 1024; // 1MB per entry
const DEFAULT_MAX_TOTAL_SIZE: usize = 128 * 1024 * 1024; // 128MB total
const DEFAULT_TTL: Duration = Duration::from_secs(30);

struct CacheEntry {
    data: Vec<u8>,
    accessed: Instant,
    inserted: Instant,
}

pub struct ReadCache {
    entries: HashMap<String, CacheEntry>,
    total_size: usize,
    max_entry_size: usize,
    max_total_size: usize,
    ttl: Duration,
}

impl ReadCache {
    pub fn new(max_total_mb: usize) -> Self {
        Self {
            entries: HashMap::new(),
            total_size: 0,
            max_entry_size: DEFAULT_MAX_ENTRY_SIZE,
            max_total_size: if max_total_mb > 0 {
                max_total_mb * 1024 * 1024
            } else {
                DEFAULT_MAX_TOTAL_SIZE
            },
            ttl: DEFAULT_TTL,
        }
    }

    pub fn get(&mut self, path: &str) -> Option<&[u8]> {
        // Check TTL first
        if let Some(entry) = self.entries.get(path) {
            if entry.inserted.elapsed() >= self.ttl {
                let size = entry.data.len();
                self.entries.remove(path);
                self.total_size -= size;
                return None;
            }
        }
        if let Some(entry) = self.entries.get_mut(path) {
            entry.accessed = Instant::now();
            Some(&entry.data)
        } else {
            None
        }
    }

    pub fn put(&mut self, path: &str, data: Vec<u8>) {
        if data.len() > self.max_entry_size {
            return;
        }

        // Remove old entry if exists
        if let Some(old) = self.entries.remove(path) {
            self.total_size -= old.data.len();
        }

        // Evict LRU entries until we have room
        while self.total_size + data.len() > self.max_total_size && !self.entries.is_empty() {
            self.evict_lru();
        }

        self.total_size += data.len();
        self.entries.insert(
            path.to_string(),
            CacheEntry {
                data,
                accessed: Instant::now(),
                inserted: Instant::now(),
            },
        );
    }

    pub fn invalidate(&mut self, path: &str) {
        if let Some(entry) = self.entries.remove(path) {
            self.total_size -= entry.data.len();
        }
    }

    pub fn invalidate_all(&mut self) {
        self.entries.clear();
        self.total_size = 0;
    }

    /// Returns true if the file is small enough to be cached.
    pub fn is_cacheable_size(&self, size: u64) -> bool {
        (size as usize) <= self.max_entry_size
    }

    fn evict_lru(&mut self) {
        let lru_key = self
            .entries
            .iter()
            .min_by_key(|(_, e)| e.accessed)
            .map(|(k, _)| k.clone());
        if let Some(key) = lru_key {
            if let Some(entry) = self.entries.remove(&key) {
                self.total_size -= entry.data.len();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_put_get() {
        let mut cache = ReadCache::new(1);
        cache.put("/a.txt", b"hello".to_vec());
        assert_eq!(cache.get("/a.txt"), Some(b"hello".as_slice()));
        assert_eq!(cache.get("/missing"), None);
    }

    #[test]
    fn invalidate_removes_entry() {
        let mut cache = ReadCache::new(1);
        cache.put("/a.txt", b"hello".to_vec());
        cache.invalidate("/a.txt");
        assert_eq!(cache.get("/a.txt"), None);
    }

    #[test]
    fn invalidate_all_clears() {
        let mut cache = ReadCache::new(1);
        cache.put("/a.txt", b"a".to_vec());
        cache.put("/b.txt", b"b".to_vec());
        cache.invalidate_all();
        assert_eq!(cache.get("/a.txt"), None);
        assert_eq!(cache.get("/b.txt"), None);
        assert_eq!(cache.total_size, 0);
    }

    #[test]
    fn oversized_entry_not_cached() {
        let mut cache = ReadCache::new(1);
        let big = vec![0u8; 2 * 1024 * 1024];
        cache.put("/big", big);
        assert_eq!(cache.get("/big"), None);
    }

    #[test]
    fn lru_eviction() {
        // Cache size = 1 byte (will normalize to 1MB in constructor, but we override)
        let mut cache = ReadCache {
            entries: HashMap::new(),
            total_size: 0,
            max_entry_size: 100,
            max_total_size: 10,
            ttl: DEFAULT_TTL,
        };
        cache.put("/a", vec![1; 5]);
        cache.put("/b", vec![2; 5]);
        // total = 10, at capacity
        assert!(cache.get("/a").is_some());
        assert!(cache.get("/b").is_some());

        // Adding /c should evict LRU
        // /b was accessed more recently (by the get above), so /a gets evicted first
        // Wait, let's be more explicit about access order:
        // After put /a and put /b, we call get /a then get /b, so /a was accessed before /b.
        // Actually both were just accessed. Let's make it deterministic:
        cache.put("/a", vec![1; 5]);
        cache.put("/b", vec![2; 5]);
        // Access /b to make /a the LRU
        let _ = cache.get("/b");
        cache.put("/c", vec![3; 5]);
        // /a should have been evicted
        assert_eq!(cache.get("/a"), None);
        assert!(cache.get("/b").is_some() || cache.get("/c").is_some());
    }

    #[test]
    fn is_cacheable_size_check() {
        let cache = ReadCache::new(1);
        assert!(cache.is_cacheable_size(100));
        assert!(cache.is_cacheable_size(1024 * 1024));
        assert!(!cache.is_cacheable_size(2 * 1024 * 1024));
    }
}
