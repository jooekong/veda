use std::collections::HashMap;
use std::time::{Duration, Instant};

const DEFAULT_MAX_ENTRY_SIZE: usize = 1024 * 1024; // 1MB per entry
const DEFAULT_MAX_TOTAL_SIZE: usize = 128 * 1024 * 1024; // 128MB total
#[cfg(test)]
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
    generation: u64,
}

impl ReadCache {
    pub fn new(max_total_mb: usize, ttl: Duration) -> Self {
        Self {
            entries: HashMap::new(),
            total_size: 0,
            max_entry_size: DEFAULT_MAX_ENTRY_SIZE,
            max_total_size: if max_total_mb > 0 {
                max_total_mb * 1024 * 1024
            } else {
                DEFAULT_MAX_TOTAL_SIZE
            },
            ttl,
            generation: 0,
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn get(&mut self, path: &str) -> Option<&[u8]> {
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

    /// Insert data only if `expected_gen` matches the current generation.
    /// Rejects stale data from reads that raced with an SSE invalidation.
    pub fn put(&mut self, path: &str, data: Vec<u8>, expected_gen: u64) {
        if expected_gen != self.generation {
            return;
        }
        if data.len() > self.max_entry_size {
            return;
        }

        if let Some(old) = self.entries.remove(path) {
            self.total_size -= old.data.len();
        }

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
        self.generation += 1;
        if let Some(entry) = self.entries.remove(path) {
            self.total_size -= entry.data.len();
        }
    }

    pub fn invalidate_all(&mut self) {
        self.generation += 1;
        self.entries.clear();
        self.total_size = 0;
    }

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
        let mut cache = ReadCache::new(1, DEFAULT_TTL);
        let gen = cache.generation();
        cache.put("/a.txt", b"hello".to_vec(), gen);
        assert_eq!(cache.get("/a.txt"), Some(b"hello".as_slice()));
        assert_eq!(cache.get("/missing"), None);
    }

    #[test]
    fn invalidate_removes_entry() {
        let mut cache = ReadCache::new(1, DEFAULT_TTL);
        let gen = cache.generation();
        cache.put("/a.txt", b"hello".to_vec(), gen);
        cache.invalidate("/a.txt");
        assert_eq!(cache.get("/a.txt"), None);
    }

    #[test]
    fn invalidate_all_clears() {
        let mut cache = ReadCache::new(1, DEFAULT_TTL);
        let gen = cache.generation();
        cache.put("/a.txt", b"a".to_vec(), gen);
        let gen = cache.generation();
        cache.put("/b.txt", b"b".to_vec(), gen);
        cache.invalidate_all();
        assert_eq!(cache.get("/a.txt"), None);
        assert_eq!(cache.get("/b.txt"), None);
        assert_eq!(cache.total_size, 0);
    }

    #[test]
    fn oversized_entry_not_cached() {
        let mut cache = ReadCache::new(1, DEFAULT_TTL);
        let gen = cache.generation();
        let big = vec![0u8; 2 * 1024 * 1024];
        cache.put("/big", big, gen);
        assert_eq!(cache.get("/big"), None);
    }

    #[test]
    fn lru_eviction() {
        let mut cache = ReadCache {
            entries: HashMap::new(),
            total_size: 0,
            max_entry_size: 100,
            max_total_size: 10,
            ttl: DEFAULT_TTL,
            generation: 0,
        };
        let gen = cache.generation();
        cache.put("/a", vec![1; 5], gen);
        let gen = cache.generation();
        cache.put("/b", vec![2; 5], gen);
        assert!(cache.get("/a").is_some());
        assert!(cache.get("/b").is_some());

        let gen = cache.generation();
        cache.put("/a", vec![1; 5], gen);
        let gen = cache.generation();
        cache.put("/b", vec![2; 5], gen);
        let _ = cache.get("/b");
        let gen = cache.generation();
        cache.put("/c", vec![3; 5], gen);
        assert_eq!(cache.get("/a"), None);
        assert!(cache.get("/b").is_some() || cache.get("/c").is_some());
    }

    #[test]
    fn is_cacheable_size_check() {
        let cache = ReadCache::new(1, DEFAULT_TTL);
        assert!(cache.is_cacheable_size(100));
        assert!(cache.is_cacheable_size(1024 * 1024));
        assert!(!cache.is_cacheable_size(2 * 1024 * 1024));
    }

    #[test]
    fn stale_generation_rejected() {
        let mut cache = ReadCache::new(1, DEFAULT_TTL);
        let gen = cache.generation();
        cache.invalidate("/other.txt");
        // gen is now stale
        cache.put("/a.txt", b"stale".to_vec(), gen);
        assert_eq!(cache.get("/a.txt"), None, "stale-gen put must be rejected");
    }

    #[test]
    fn generation_increments_on_invalidate() {
        let mut cache = ReadCache::new(1, DEFAULT_TTL);
        let g0 = cache.generation();
        cache.invalidate("/x");
        assert_eq!(cache.generation(), g0 + 1);
        cache.invalidate_all();
        assert_eq!(cache.generation(), g0 + 2);
    }
}
