mod cache;

use cache::Cache;

fn main() {
    let mut cache = Cache::new();
    cache.insert("alpha", 3);
    cache.insert("beta", 12);
    cache.insert("gamma", 25);
    cache.evict_large(10);
    println!("{} entries left", cache.len());
}
