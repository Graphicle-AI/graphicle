//! Feature-cache eviction under a working set larger than the cache. Its own
//! test binary: it fills the process-wide cache.
use graphicle_memory::cache;
use std::sync::Arc;

#[test]
fn a_full_cache_keeps_recently_used_entries_instead_of_cycling() {
    let scope = "cache-policy";
    let max = cache::stats().unwrap()["max_entries"].as_u64().unwrap() as usize;
    let first = cache::features(scope, "entry 0 alpha").unwrap();
    for i in 1..max {
        cache::features(scope, &format!("entry {i} alpha")).unwrap();
    }
    assert_eq!(
        cache::stats().unwrap()["entries"].as_u64().unwrap() as usize,
        max
    );
    // Plain LRU would now evict entry 0 for every newcomer and, over a
    // repeated pass, miss on every lookup. Recently used entries stay; the
    // newcomer is computed and returned without being cached.
    let before = cache::stats().unwrap()["misses"].as_u64().unwrap();
    let newcomer = cache::features(scope, "one entry too many").unwrap();
    let again = cache::features(scope, "one entry too many").unwrap();
    assert!(!Arc::ptr_eq(&newcomer, &again));
    assert_eq!(newcomer.length, again.length);
    assert_eq!(
        cache::stats().unwrap()["misses"].as_u64().unwrap(),
        before + 2
    );
    let still = cache::features(scope, "entry 0 alpha").unwrap();
    assert!(Arc::ptr_eq(&first, &still));
    // Forgetting the scope frees the room at once.
    cache::clear(scope).unwrap();
    let admitted = cache::features(scope, "one entry too many").unwrap();
    assert!(Arc::ptr_eq(
        &admitted,
        &cache::features(scope, "one entry too many").unwrap()
    ));
    cache::clear(scope).unwrap();
}
