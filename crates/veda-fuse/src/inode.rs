use std::collections::HashMap;
use std::time::{Duration, Instant};

use fuser::FileAttr;

pub const ROOT_INO: u64 = 1;

pub struct InodeTable {
    next_ino: u64,
    ino_to_path: HashMap<u64, String>,
    path_to_ino: HashMap<String, u64>,
    attr_cache: HashMap<u64, (FileAttr, Instant)>,
    attr_ttl: Duration,
    nlookup: HashMap<u64, u64>,
}

impl InodeTable {
    pub fn new() -> Self {
        Self::new_with_ttl(Duration::from_secs(5))
    }

    pub fn new_with_ttl(attr_ttl: Duration) -> Self {
        let mut t = Self {
            next_ino: 2,
            ino_to_path: HashMap::new(),
            path_to_ino: HashMap::new(),
            attr_cache: HashMap::new(),
            attr_ttl,
            nlookup: HashMap::new(),
        };
        t.ino_to_path.insert(ROOT_INO, "/".to_string());
        t.path_to_ino.insert("/".to_string(), ROOT_INO);
        t
    }

    pub fn get_or_create_ino(&mut self, path: &str) -> u64 {
        if let Some(&ino) = self.path_to_ino.get(path) {
            return ino;
        }
        let ino = self.next_ino;
        self.next_ino += 1;
        self.ino_to_path.insert(ino, path.to_string());
        self.path_to_ino.insert(path.to_string(), ino);
        ino
    }

    pub fn get_path(&self, ino: u64) -> Option<&str> {
        self.ino_to_path.get(&ino).map(|s| s.as_str())
    }

    pub fn get_ino(&self, path: &str) -> Option<u64> {
        self.path_to_ino.get(path).copied()
    }

    pub fn get_cached_attr(&self, ino: u64) -> Option<FileAttr> {
        if let Some((attr, ts)) = self.attr_cache.get(&ino) {
            if ts.elapsed() < self.attr_ttl {
                return Some(*attr);
            }
        }
        None
    }

    pub fn set_cached_attr(&mut self, ino: u64, attr: FileAttr) {
        self.attr_cache.insert(ino, (attr, Instant::now()));
    }

    pub fn invalidate(&mut self, ino: u64) {
        self.attr_cache.remove(&ino);
    }

    pub fn remove_path(&mut self, path: &str) {
        if let Some(ino) = self.path_to_ino.remove(path) {
            self.ino_to_path.remove(&ino);
            self.attr_cache.remove(&ino);
            self.nlookup.remove(&ino);
        }
    }

    pub fn rename_path(&mut self, old: &str, new: &str) {
        if let Some(ino) = self.path_to_ino.remove(old) {
            self.ino_to_path.insert(ino, new.to_string());
            self.path_to_ino.insert(new.to_string(), ino);
            self.attr_cache.remove(&ino);
        }
        let prefix = format!("{old}/");
        let children: Vec<(String, u64)> = self
            .path_to_ino
            .iter()
            .filter(|(p, _)| p.starts_with(&prefix))
            .map(|(p, &ino)| (p.clone(), ino))
            .collect();
        for (child_path, ino) in children {
            self.path_to_ino.remove(&child_path);
            let new_child = format!("{new}{}", &child_path[old.len()..]);
            self.ino_to_path.insert(ino, new_child.clone());
            self.path_to_ino.insert(new_child, ino);
            self.attr_cache.remove(&ino);
        }
    }

    pub fn inc_nlookup(&mut self, ino: u64) {
        *self.nlookup.entry(ino).or_insert(0) += 1;
    }

    /// Decrement kernel reference count. Reclaims inode mapping when count
    /// reaches zero (root inode is never reclaimed).
    pub fn forget(&mut self, ino: u64, nlookup: u64) {
        if ino == ROOT_INO {
            return;
        }
        if let Some(count) = self.nlookup.get_mut(&ino) {
            *count = count.saturating_sub(nlookup);
            if *count == 0 {
                self.nlookup.remove(&ino);
                if let Some(path) = self.ino_to_path.remove(&ino) {
                    self.path_to_ino.remove(&path);
                }
                self.attr_cache.remove(&ino);
            }
        }
    }

    #[cfg(test)]
    pub fn nlookup_count(&self, ino: u64) -> u64 {
        self.nlookup.get(&ino).copied().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forget_reclaims_inode() {
        let mut t = InodeTable::new();
        let ino = t.get_or_create_ino("/a.txt");
        t.inc_nlookup(ino);
        t.inc_nlookup(ino);
        assert_eq!(t.nlookup_count(ino), 2);

        t.forget(ino, 1);
        assert_eq!(t.nlookup_count(ino), 1);
        assert!(t.get_path(ino).is_some());

        t.forget(ino, 1);
        assert_eq!(t.nlookup_count(ino), 0);
        assert!(t.get_path(ino).is_none());
        assert!(t.get_ino("/a.txt").is_none());
    }

    #[test]
    fn forget_root_is_noop() {
        let mut t = InodeTable::new();
        t.forget(ROOT_INO, 100);
        assert!(t.get_path(ROOT_INO).is_some());
    }

    #[test]
    fn forget_after_remove_path() {
        let mut t = InodeTable::new();
        let ino = t.get_or_create_ino("/a.txt");
        t.inc_nlookup(ino);
        t.remove_path("/a.txt");
        // Path mapping already gone, forget just cleans up nlookup
        t.forget(ino, 1);
        assert_eq!(t.nlookup_count(ino), 0);
    }
}
