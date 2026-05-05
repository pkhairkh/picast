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
        Self {
            entries: HashMap::new(),
            ttl: DEFAULT_TTL,
            max_entries: MAX_ENTRIES,
        }
    }

    /// Create a new cache with a custom TTL.
    pub fn with_ttl(ttl: Duration) -> Self {
        Self {
            entries: HashMap::new(),
            ttl,
            max_entries: MAX_ENTRIES,
        }
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
            if let Some(oldest_key) = self
                .entries
                .iter()
                .min_by_key(|(_, v)| v.inserted_at)
                .map(|(k, _)| k.clone())
            {
                self.entries.remove(&oldest_key);
            }
        }

        self.entries.insert(
            url.to_owned(),
            CacheEntry {
                result,
                inserted_at: Instant::now(),
            },
        );
    }

    /// Look up a URL in the cache.
    ///
    /// Returns `Some(&ResolveResult)` if the URL is cached and
    /// hasn't expired, `None` otherwise.
    pub fn get(&mut self, url: &str) -> Option<&ResolveResult> {
        let now = Instant::now();

        if let Some(entry) = self.entries.get(url) {
            if now.duration_since(entry.inserted_at) < self.ttl {
                return Some(&entry.result);
            } else {
                // Entry expired — remove it.
                self.entries.remove(url);
            }
        }

        None
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
}
