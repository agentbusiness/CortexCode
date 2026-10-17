/// A tiny in-memory cache of named counters.
pub struct Cache {
    entries: Vec<(String, u32)>,
    evictions: u32,
}

impl Cache {
    pub fn new() -> Self {
        Self { entries: Vec::new(), evictions: 0 }
    }

    pub fn insert(&mut self, key: &str, value: u32) {
        self.entries.push((key.to_string(), value));
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Drop every entry whose value is above `limit`.
    pub fn evict_large(&mut self, limit: u32) {
        for (key, value) in self.entries.iter() {
            if *value > limit {
                self.evict(key);
            }
        }
    }

    fn evict(&mut self, key: &str) {
        self.entries.retain(|(k, _)| k != key);
        self.evictions += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evicts_only_large_values() {
        let mut cache = Cache::new();
        cache.insert("a", 1);
        cache.insert("b", 50);
        cache.evict_large(10);
        assert_eq!(cache.len(), 1);
    }
}
