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
}
