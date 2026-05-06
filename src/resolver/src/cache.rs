//! PiCast Resolver Cache
//!
//! Simple in-memory cache for resolved URLs. Prevents duplicate
//! resolution of the same URL within a configurable TTL window.
//!
//! The cache is intentionally simple — no LRU eviction, no disk
//! persistence. For v1 this is sufficient; a more sophisticated
//! cache can be added in v2 if needed.

use crate::ResolveResult;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Default cache TTL: 10 minutes.
const DEFAULT_TTL: Duration = Duration::from_secs(600);

/// Maximum number of entries in the cache.
const MAX_ENTRIES: usize = 256;

/// A cached resolution result with its insertion time.
struct CacheEntry {
    result: ResolveResult,
    inserted_at: Instant,
}

/// In-memory URL resolution cache.
///
/// Thread safety: This cache is **not** thread-safe. It should be
/// wrapped in a `Mutex` or `RwLock` if shared across tasks.
pub struct ResolveCache {
    entries: HashMap<String, CacheEntry>,
    ttl: Duration,
    max_entries: usize,
}

impl ResolveCache {
    /// Create a new cache with default TTL (10 minutes).
    pub fn new() -> Self {
        Self { entries: HashMap::new(), ttl: DEFAULT_TTL, max_entries: MAX_ENTRIES }
    }

    /// Create a new cache with a custom TTL.
    pub fn with_ttl(ttl: Duration) -> Self {
        Self { entries: HashMap::new(), ttl, max_entries: MAX_ENTRIES }
    }

    /// Insert a resolution result into the cache.
    ///
    /// If the cache is full, expired entries are evicted first.
    /// If still full after eviction, the oldest entry is removed.
    pub fn insert(&mut self, url: &str, result: ResolveResult) {
        // Evict expired entries if we're at capacity.
        if self.entries.len() >= self.max_entries {
            self.evict_expired();
        }

        // If still at capacity, remove the oldest entry.
        if self.entries.len() >= self.max_entries {
            if let Some(oldest_key) =
                self.entries.iter().min_by_key(|(_, v)| v.inserted_at).map(|(k, _)| k.clone())
            {
                self.entries.remove(&oldest_key);
            }
        }

        self.entries.insert(url.to_owned(), CacheEntry { result, inserted_at: Instant::now() });
    }

    /// Look up a URL in the cache.
    ///
    /// Returns `Some(&ResolveResult)` if the URL is cached and
    /// hasn't expired, `None` otherwise.
    pub fn get(&mut self, url: &str) -> Option<&ResolveResult> {
        let now = Instant::now();

        // Check if the entry exists and is still valid.
        let is_valid = self
            .entries
            .get(url)
            .is_some_and(|entry| now.duration_since(entry.inserted_at) < self.ttl);

        if !is_valid {
            // Remove expired entry if it exists.
            self.entries.remove(url);
            return None;
        }

        // Entry is valid — return a reference.
        Some(&self.entries.get(url).unwrap().result)
    }

    /// Remove all expired entries from the cache.
    pub fn evict_expired(&mut self) {
        let now = Instant::now();
        self.entries.retain(|_, entry| now.duration_since(entry.inserted_at) < self.ttl);
    }

    /// Return the number of entries in the cache (including expired).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Return whether the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Clear all entries from the cache.
    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

impl Default for ResolveCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::UrlCategory;

    fn test_result(url: &str) -> ResolveResult {
        ResolveResult {
            source_url: url.to_owned(),
            direct_url: format!("{}?direct=1", url),
            category: UrlCategory::DirectMedia,
            mime_type: Some("video/mp4".into()),
            content_length: None,
            used_tor: false,
            title: Some("Test Video".into()),
            duration: Some(300000),
            thumbnail: None,
            vcodec: Some("avc1".into()),
            acodec: Some("mp4a".into()),
            width: Some(1920),
            height: Some(1080),
            subtitle_tracks: vec![],
        }
    }

    #[test]
    fn cache_insert_and_get() {
        let mut cache = ResolveCache::new();
        let url = "https://example.com/video.mp4";
        let result = test_result(url);

        cache.insert(url, result);
        assert_eq!(cache.len(), 1);

        let cached = cache.get(url);
        assert!(cached.is_some());
        assert_eq!(cached.unwrap().direct_url, "https://example.com/video.mp4?direct=1");
    }

    #[test]
    fn cache_miss() {
        let mut cache = ResolveCache::new();
        assert!(cache.get("https://example.com/not-found").is_none());
    }

    #[test]
    fn cache_expiry() {
        let mut cache = ResolveCache::with_ttl(Duration::from_millis(10));
        let url = "https://example.com/video.mp4";

        cache.insert(url, test_result(url));

        // Wait for the entry to expire.
        std::thread::sleep(Duration::from_millis(20));

        // The entry should be expired.
        assert!(cache.get(url).is_none());
    }

    #[test]
    fn cache_evict_expired() {
        let mut cache = ResolveCache::with_ttl(Duration::from_millis(10));
        let url1 = "https://example.com/video1.mp4";
        let url2 = "https://example.com/video2.mp4";

        cache.insert(url1, test_result(url1));
        std::thread::sleep(Duration::from_millis(20));
        cache.insert(url2, test_result(url2));

        assert_eq!(cache.len(), 2);

        cache.evict_expired();
        assert_eq!(cache.len(), 1);

        // url1 should be expired, url2 should still be there.
        assert!(cache.get(url1).is_none());
        assert!(cache.get(url2).is_some());
    }

    #[test]
    fn cache_max_entries() {
        let mut cache = ResolveCache::new();
        // Override max_entries for testing.
        cache.max_entries = 3;

        for i in 0..5 {
            let url = format!("https://example.com/video{}.mp4", i);
            cache.insert(&url, test_result(&url));
        }

        // Cache should have evicted some entries.
        assert!(cache.len() <= 3);
    }

    #[test]
    fn cache_clear() {
        let mut cache = ResolveCache::new();
        cache.insert("url1", test_result("url1"));
        cache.insert("url2", test_result("url2"));
        assert_eq!(cache.len(), 2);

        cache.clear();
        assert!(cache.is_empty());
    }

    #[test]
    fn cache_default() {
        let cache = ResolveCache::default();
        assert!(cache.is_empty());
    }

    // ── Comprehensive cache tests ──────────────────────────────────────

    #[test]
    fn cache_ttl_expiration_with_very_short_ttl() {
        // Use a 1ms TTL to verify entries expire almost immediately.
        let mut cache = ResolveCache::with_ttl(Duration::from_millis(1));
        let url = "https://example.com/video.mp4";

        cache.insert(url, test_result(url));
        assert_eq!(cache.len(), 1, "cache should contain the entry immediately after insert");

        // Wait for the TTL to expire.
        std::thread::sleep(Duration::from_millis(5));

        // The entry should be expired and removed on get.
        assert!(cache.get(url).is_none(), "entry should be expired after 1ms TTL");
        assert_eq!(cache.len(), 0, "cache should be empty after expired entry is accessed and removed");
    }

    #[test]
    fn cache_ttl_not_expired_within_window() {
        // Use a generous TTL to ensure entries are still valid.
        let mut cache = ResolveCache::with_ttl(Duration::from_secs(600));
        let url = "https://example.com/video.mp4";

        cache.insert(url, test_result(url));

        // Should still be present immediately.
        assert!(cache.get(url).is_some(), "entry should be valid within TTL window");
    }

    #[test]
    fn cache_lru_eviction_oldest_removed_first() {
        let mut cache = ResolveCache::new();
        cache.max_entries = 3;

        // Insert entries 0, 1, 2 to fill the cache.
        for i in 0..3 {
            let url = format!("https://example.com/video{}.mp4", i);
            cache.insert(&url, test_result(&url));
        }
        assert_eq!(cache.len(), 3, "cache should be at max capacity");

        // Insert a 4th entry — the oldest (video0) should be evicted.
        let url3 = "https://example.com/video3.mp4";
        cache.insert(url3, test_result(url3));

        // The cache should still be at max_capacity (or less after eviction).
        assert!(cache.len() <= 3, "cache should not exceed max_entries");

        // video0 (the oldest) should be gone.
        assert!(cache.get("https://example.com/video0.mp4").is_none(),
                "oldest entry should be evicted when cache is full");

        // video3 (the newest) should be present.
        assert!(cache.get(url3).is_some(),
                "newest entry should be present after eviction");
    }

    #[test]
    fn cache_lru_eviction_with_all_expired() {
        // When the cache is at capacity and all entries are expired,
        // inserting a new one should evict all expired entries first.
        let mut cache = ResolveCache::with_ttl(Duration::from_millis(1));
        cache.max_entries = 3;

        // Fill the cache to capacity with entries that will expire.
        for i in 0..3 {
            let url = format!("https://example.com/old{}.mp4", i);
            cache.insert(&url, test_result(&url));
        }
        assert_eq!(cache.len(), 3, "cache should be at max capacity");

        // Wait for them to expire.
        std::thread::sleep(Duration::from_millis(5));

        // Insert a new entry — since we're at capacity, expired entries
        // should be evicted first, making room for the new one.
        let new_url = "https://example.com/new.mp4";
        cache.insert(new_url, test_result(new_url));

        // Only the new entry should remain (all 3 expired entries evicted, 1 new inserted).
        assert_eq!(cache.len(), 1, "only the new entry should remain after expired eviction");
        assert!(cache.get(new_url).is_some());
    }

    #[test]
    fn cache_size_tracking_after_insertions() {
        let mut cache = ResolveCache::new();
        assert_eq!(cache.len(), 0, "new cache should be empty");

        cache.insert("url1", test_result("url1"));
        assert_eq!(cache.len(), 1);

        cache.insert("url2", test_result("url2"));
        assert_eq!(cache.len(), 2);

        cache.insert("url3", test_result("url3"));
        assert_eq!(cache.len(), 3);
    }

    #[test]
    fn cache_size_tracking_after_evictions() {
        let mut cache = ResolveCache::new();
        cache.max_entries = 5;

        for i in 0..5 {
            let url = format!("https://example.com/video{}.mp4", i);
            cache.insert(&url, test_result(&url));
        }
        assert_eq!(cache.len(), 5, "cache should be at max capacity");

        // Insert one more — should trigger eviction of oldest.
        cache.insert("https://example.com/video5.mp4", test_result("https://example.com/video5.mp4"));
        assert!(cache.len() <= 5, "cache should not exceed max capacity after eviction");
    }

    #[test]
    fn cache_size_tracking_after_expiry() {
        let mut cache = ResolveCache::with_ttl(Duration::from_millis(1));

        for i in 0..5 {
            let url = format!("https://example.com/video{}.mp4", i);
            cache.insert(&url, test_result(&url));
        }
        assert_eq!(cache.len(), 5);

        // Wait for all to expire.
        std::thread::sleep(Duration::from_millis(5));

        // evict_expired should remove all.
        cache.evict_expired();
        assert_eq!(cache.len(), 0, "all expired entries should be removed");
    }

    #[test]
    fn cache_clear_empties_all_entries() {
        let mut cache = ResolveCache::new();

        // Insert several entries.
        for i in 0..10 {
            let url = format!("https://example.com/video{}.mp4", i);
            cache.insert(&url, test_result(&url));
        }
        assert_eq!(cache.len(), 10, "cache should have 10 entries before clear");

        cache.clear();

        assert!(cache.is_empty(), "cache should be empty after clear");
        assert_eq!(cache.len(), 0, "len() should return 0 after clear");

        // Verify individual lookups also return None.
        for i in 0..10 {
            let url = format!("https://example.com/video{}.mp4", i);
            assert!(cache.get(&url).is_none(), "entries should not be found after clear");
        }
    }

    #[test]
    fn cache_clear_then_reinsert() {
        let mut cache = ResolveCache::new();

        cache.insert("url1", test_result("url1"));
        cache.clear();
        assert!(cache.is_empty());

        // Should be able to insert again after clear.
        cache.insert("url2", test_result("url2"));
        assert_eq!(cache.len(), 1);
        assert!(cache.get("url2").is_some());
    }

    #[test]
    fn cache_insert_overwrites_existing_key() {
        let mut cache = ResolveCache::new();
        let url = "https://example.com/video.mp4";

        let mut result1 = test_result(url);
        result1.title = Some("First Title".into());
        cache.insert(url, result1);

        let mut result2 = test_result(url);
        result2.title = Some("Second Title".into());
        cache.insert(url, result2);

        // Should have only 1 entry (overwritten, not duplicated).
        assert_eq!(cache.len(), 1);

        // Should return the most recent value.
        let cached = cache.get(url).unwrap();
        assert_eq!(cached.title, Some("Second Title".into()));
    }

    #[test]
    fn cache_get_removes_expired_entry() {
        let mut cache = ResolveCache::with_ttl(Duration::from_millis(1));
        let url = "https://example.com/video.mp4";

        cache.insert(url, test_result(url));
        assert_eq!(cache.len(), 1);

        // Wait for expiry.
        std::thread::sleep(Duration::from_millis(5));

        // get() should remove the expired entry.
        assert!(cache.get(url).is_none());
        assert_eq!(cache.len(), 0, "expired entry should be removed from cache on get()");
    }

    #[test]
    fn cache_is_empty_on_new() {
        let cache = ResolveCache::new();
        assert!(cache.is_empty());
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn cache_with_ttl_custom_duration() {
        let cache = ResolveCache::with_ttl(Duration::from_secs(3600));
        assert!(cache.is_empty());
        // TTL is stored internally; we verify it works by inserting and
        // confirming the entry is still there well within the TTL.
        drop(cache); // Just verifying construction works without panic.
    }

    #[test]
    fn cache_multiple_entries_independent_expiry() {
        let mut cache = ResolveCache::with_ttl(Duration::from_millis(50));

        let url1 = "https://example.com/video1.mp4";
        cache.insert(url1, test_result(url1));

        // Wait a bit, then insert a second entry.
        std::thread::sleep(Duration::from_millis(30));
        let url2 = "https://example.com/video2.mp4";
        cache.insert(url2, test_result(url2));

        // url1 should be close to expiry but url2 should still be fresh.
        // Wait a bit more so url1 expires but url2 is still valid.
        std::thread::sleep(Duration::from_millis(30));

        assert!(cache.get(url1).is_none(), "url1 should have expired");
        assert!(cache.get(url2).is_some(), "url2 should still be valid");
    }
}
