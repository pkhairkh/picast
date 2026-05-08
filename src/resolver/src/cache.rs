//! PiCast Resolver Cache
//!
//! SQLite-backed cache for resolved URLs. Prevents duplicate
//! resolution of the same URL within a configurable TTL window.
//!
//! The cache stores resolution results in a SQLite database with WAL
//! mode for concurrent access. Entries older than the TTL are
//! automatically cleaned up on each insert.
//!
//! Timestamps are stored as Unix epoch seconds (INTEGER) for
//! reliable and portable time comparisons.

use crate::ResolveResult;
use rusqlite::{params, Connection};
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Default cache TTL: 10 minutes.
const DEFAULT_TTL: Duration = Duration::from_secs(600);

/// Maximum age for stale entries before cleanup (1 hour).
const CLEANUP_AGE: Duration = Duration::from_secs(3600);

/// Return the current time as Unix epoch seconds.
fn now_epoch_secs() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64
}

/// SQLite-backed URL resolution cache.
///
/// Thread safety: The cache wraps a `Mutex<Connection>` so it can be
/// shared across async tasks. SQLite is compiled with WAL mode for
/// concurrent read access.
pub struct ResolveCache {
    /// SQLite connection (guarded by Mutex for thread safety).
    conn: Mutex<Connection>,
    /// TTL for cache entries.
    ttl: Duration,
}

impl ResolveCache {
    /// Create a new in-memory SQLite cache with default TTL (10 minutes).
    pub fn new() -> Self {
        Self::with_path_and_ttl(None, DEFAULT_TTL)
    }

    /// Create a new cache with a custom TTL.
    pub fn with_ttl(ttl: Duration) -> Self {
        Self::with_path_and_ttl(None, ttl)
    }

    /// Create a new cache backed by a SQLite database file at `path`.
    ///
    /// If `path` is `None`, an in-memory database is used.
    pub fn with_path(path: &Path) -> Self {
        Self::with_path_and_ttl(Some(path), DEFAULT_TTL)
    }

    /// Create a new cache backed by a SQLite database file with custom TTL.
    ///
    /// If `path` is `None`, an in-memory database is used.
    pub fn with_path_and_ttl(path: Option<&Path>, ttl: Duration) -> Self {
        let conn = match path {
            Some(p) => Connection::open(p).expect("failed to open cache database"),
            None => Connection::open_in_memory().expect("failed to create in-memory database"),
        };

        // Enable WAL mode for concurrent read access.
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")
            .expect("failed to set WAL mode");

        // Create the cache table. resolved_at is Unix epoch seconds.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS resolved_urls (
                source_url     TEXT PRIMARY KEY,
                direct_url     TEXT NOT NULL,
                audio_url      TEXT,
                category       TEXT NOT NULL,
                mime_type      TEXT,
                content_length INTEGER,
                used_tor       INTEGER NOT NULL,
                title          TEXT,
                duration       INTEGER,
                thumbnail      TEXT,
                vcodec         TEXT,
                acodec         TEXT,
                width          INTEGER,
                height         INTEGER,
                subtitle_tracks TEXT,
                resolved_at    INTEGER NOT NULL
            );",
        )
        .expect("failed to create cache table");

        // Schema migration: add audio_url column if it doesn't exist.
        // This column was added after the initial schema, so databases
        // created by earlier versions won't have it.  CREATE TABLE IF
        // NOT EXISTS doesn't alter existing tables, so we need an
        // explicit migration.  SQLite doesn't support IF NOT EXISTS on
        // ALTER TABLE ADD COLUMN (until 3.35.0), so we just try it and
        // ignore the "duplicate column" error.
        if let Err(e) = conn.execute("ALTER TABLE resolved_urls ADD COLUMN audio_url TEXT", []) {
            let msg = e.to_string();
            if !msg.contains("duplicate column") {
                tracing::warn!(error = %msg, "cache migration: unexpected error adding audio_url column");
            }
            // "duplicate column" is expected when the column already exists — no action needed.
        }

        Self { conn: Mutex::new(conn), ttl }
    }

    /// Insert a resolution result into the cache.
    ///
    /// If an entry with the same `source_url` already exists, it is
    /// replaced. Stale entries (older than 1 hour) are cleaned up
    /// on each insert.
    pub fn insert(&self, _url: &str, result: ResolveResult) {
        let conn = self.conn.lock().unwrap();
        self.insert_with_conn(&conn, _url, &result);
    }

    /// Look up a URL in the cache.
    ///
    /// Returns `Some(ResolveResult)` if the URL is cached and
    /// hasn't expired, `None` otherwise.
    pub fn get(&self, url: &str) -> Option<ResolveResult> {
        let conn = self.conn.lock().unwrap();
        let now = now_epoch_secs();
        let ttl_secs = self.ttl.as_secs() as i64;
        let cutoff = now - ttl_secs;

        let mut stmt = conn
            .prepare(
                "SELECT source_url, direct_url, audio_url, category, mime_type, content_length,
                    used_tor, title, duration, thumbnail, vcodec, acodec,
                    width, height, subtitle_tracks
             FROM resolved_urls
             WHERE source_url = ?
               AND resolved_at > ?",
            )
            .ok()?;

        let result = stmt.query_row(params![url, cutoff], |row| Ok(row_to_resolve_result(row)));

        match result {
            Ok(r) => Some(r),
            Err(rusqlite::Error::QueryReturnedNoRows) => None,
            Err(e) => {
                tracing::warn!(error = %e, "cache query error");
                None
            },
        }
    }

    /// Remove all expired entries from the cache.
    pub fn evict_expired(&self) {
        let conn = self.conn.lock().unwrap();
        let now = now_epoch_secs();
        let ttl_secs = self.ttl.as_secs() as i64;
        let cutoff = now - ttl_secs;

        match conn.execute("DELETE FROM resolved_urls WHERE resolved_at < ?", params![cutoff]) {
            Ok(deleted) => {
                if deleted > 0 {
                    tracing::debug!(deleted = deleted, "evicted expired cache entries");
                }
            },
            Err(e) => tracing::warn!(error = %e, "failed to evict expired cache entries"),
        }
    }

    /// Return the number of entries in the cache (including expired).
    pub fn len(&self) -> usize {
        let conn = self.conn.lock().unwrap();
        conn.query_row("SELECT COUNT(*) FROM resolved_urls", [], |row| row.get::<_, i64>(0))
            .unwrap_or(0) as usize
    }

    /// Return whether the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Clear all entries from the cache.
    pub fn clear(&self) {
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute("DELETE FROM resolved_urls", []);
    }

    /// Delete a specific entry from the cache by source URL.
    ///
    /// Used when the CDN returns 403 Forbidden due to an IP-bound token
    /// that no longer matches the current Tor exit IP. Deleting the entry
    /// forces the next `resolve()` call to make a fresh network request,
    /// which will get a URL bound to the current exit IP.
    pub fn delete(&self, url: &str) {
        let conn = self.conn.lock().unwrap();
        match conn.execute("DELETE FROM resolved_urls WHERE source_url = ?", params![url]) {
            Ok(deleted) => {
                if deleted > 0 {
                    tracing::info!(url = url, deleted = deleted, "deleted cache entry for re-resolve");
                }
            },
            Err(e) => tracing::warn!(error = %e, url = url, "failed to delete cache entry"),
        }
    }

    /// Clean up entries older than the cleanup age (1 hour).
    fn cleanup_stale(&self, conn: &Connection) {
        let now = now_epoch_secs();
        let cleanup_secs = CLEANUP_AGE.as_secs() as i64;
        let cutoff = now - cleanup_secs;
        let _ = conn.execute("DELETE FROM resolved_urls WHERE resolved_at < ?", params![cutoff]);
    }

    /// Look up a URL in the cache, or insert it using the provided closure.
    ///
    /// This is atomic — the lock is held across the get and insert,
    /// preventing TOCTOU races where two tasks resolve the same URL.
    pub fn get_or_insert_with<F>(&self, url: &str, f: F) -> Option<ResolveResult>
    where
        F: FnOnce() -> ResolveResult,
    {
        let conn = self.conn.lock().unwrap();
        let now = now_epoch_secs();
        let ttl_secs = self.ttl.as_secs() as i64;
        let cutoff = now - ttl_secs;

        // Try to get from cache first.
        let mut stmt = match conn.prepare(
            "SELECT source_url, direct_url, audio_url, category, mime_type, content_length,
                used_tor, title, duration, thumbnail, vcodec, acodec,
                width, height, subtitle_tracks
             FROM resolved_urls
             WHERE source_url = ?
               AND resolved_at > ?",
        ) {
            Ok(stmt) => stmt,
            Err(e) => {
                tracing::warn!(error = %e, "cache prepare error");
                return None;
            },
        };

        let result = stmt.query_row(params![url, cutoff], |row| Ok(row_to_resolve_result(row)));

        match result {
            Ok(r) => Some(r),
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                // Cache miss — compute and insert atomically.
                let computed = f();
                self.insert_with_conn(&conn, url, &computed);
                Some(computed)
            },
            Err(e) => {
                tracing::warn!(error = %e, "cache query error");
                None
            },
        }
    }

    /// Insert a cache entry using an existing locked connection.
    fn insert_with_conn(&self, conn: &Connection, _url: &str, result: &ResolveResult) {
        let now = now_epoch_secs();
        let subtitle_json =
            serde_json::to_string(&result.subtitle_tracks).unwrap_or_else(|_| "[]".into());

        let content_length: Option<i64> = result.content_length.map(|v| v as i64);
        let duration: Option<i64> = result.duration.map(|v| v as i64);
        let width: Option<i32> = result.width.map(|v| v as i32);
        let height: Option<i32> = result.height.map(|v| v as i32);

        if let Err(e) = conn.execute(
            "INSERT OR REPLACE INTO resolved_urls
                (source_url, direct_url, audio_url, category, mime_type, content_length,
                 used_tor, title, duration, thumbnail, vcodec, acodec,
                 width, height, subtitle_tracks, resolved_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                result.source_url,
                result.direct_url,
                result.audio_url,
                result.category.to_string(),
                result.mime_type,
                content_length,
                result.used_tor as i32,
                result.title,
                duration,
                result.thumbnail,
                result.vcodec,
                result.acodec,
                width,
                height,
                subtitle_json,
                now,
            ],
        ) {
            tracing::warn!(error = %e, "failed to insert cache entry");
        }

        self.cleanup_stale(conn);
    }
}

/// Convert a database row into a `ResolveResult`.
fn row_to_resolve_result(row: &rusqlite::Row<'_>) -> ResolveResult {
    let category_str: String = row.get(3).unwrap_or_else(|_| "direct_media".into());
    let category = crate::UrlCategory::from_str(&category_str);

    let used_tor: i32 = row.get(6).unwrap_or(0);

    let subtitle_tracks_str: String = row.get(14).unwrap_or_else(|_| "[]".into());
    let subtitle_tracks: Vec<String> =
        serde_json::from_str(&subtitle_tracks_str).unwrap_or_default();

    // Convert i64/i32 back to u64/u32 for ResolveResult fields.
    let content_length: Option<u64> =
        row.get::<_, Option<i64>>(5).unwrap_or(None).map(|v| v as u64);
    let duration: Option<u64> = row.get::<_, Option<i64>>(8).unwrap_or(None).map(|v| v as u64);
    let width: Option<u32> = row.get::<_, Option<i32>>(12).unwrap_or(None).map(|v| v as u32);
    let height: Option<u32> = row.get::<_, Option<i32>>(13).unwrap_or(None).map(|v| v as u32);

    ResolveResult {
        source_url: row.get(0).unwrap_or_default(),
        direct_url: row.get(1).unwrap_or_default(),
        audio_url: row.get(2).unwrap_or(None),
        category,
        mime_type: row.get(4).unwrap_or(None),
        content_length,
        used_tor: used_tor != 0,
        title: row.get(7).unwrap_or(None),
        duration,
        thumbnail: row.get(9).unwrap_or(None),
        vcodec: row.get(10).unwrap_or(None),
        acodec: row.get(11).unwrap_or(None),
        width,
        height,
        subtitle_tracks,
        cookies: vec![],
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
            audio_url: None,
            category: UrlCategory::DirectMedia,
            mime_type: Some("video/mp4".into()),
            content_length: Some(1024),
            used_tor: false,
            title: Some("Test Video".into()),
            duration: Some(300000),
            thumbnail: None,
            vcodec: Some("avc1".into()),
            acodec: Some("mp4a".into()),
            width: Some(1920),
            height: Some(1080),
            subtitle_tracks: vec!["en".into(), "es".into()],
            cookies: vec![],
        }
    }

    fn test_result_with_tor(url: &str) -> ResolveResult {
        let mut r = test_result(url);
        r.used_tor = true;
        r.category = UrlCategory::WebPage;
        r.title = Some("YouTube Video".into());
        r
    }

    #[test]
    fn cache_insert_and_get() {
        let cache = ResolveCache::new();
        let url = "https://example.com/video.mp4";
        let result = test_result(url);

        cache.insert(url, result);
        assert_eq!(cache.len(), 1);

        let cached = cache.get(url);
        assert!(cached.is_some());
        let cached = cached.unwrap();
        assert_eq!(cached.direct_url, "https://example.com/video.mp4?direct=1");
        assert_eq!(cached.category, UrlCategory::DirectMedia);
        assert_eq!(cached.mime_type, Some("video/mp4".into()));
        assert_eq!(cached.content_length, Some(1024));
        assert!(!cached.used_tor);
        assert_eq!(cached.title, Some("Test Video".into()));
        assert_eq!(cached.duration, Some(300000));
        assert_eq!(cached.vcodec, Some("avc1".into()));
        assert_eq!(cached.acodec, Some("mp4a".into()));
        assert_eq!(cached.width, Some(1920));
        assert_eq!(cached.height, Some(1080));
        assert_eq!(cached.subtitle_tracks, vec!["en", "es"]);
    }

    #[test]
    fn cache_miss() {
        let cache = ResolveCache::new();
        assert!(cache.get("https://example.com/not-found").is_none());
    }

    #[test]
    fn cache_insert_overwrites_existing_key() {
        let cache = ResolveCache::new();
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
    fn cache_tor_result_roundtrip() {
        let cache = ResolveCache::new();
        let url = "https://www.youtube.com/watch?v=abc";
        let result = test_result_with_tor(url);

        cache.insert(url, result);

        let cached = cache.get(url).unwrap();
        assert!(cached.used_tor);
        assert_eq!(cached.category, UrlCategory::WebPage);
        assert_eq!(cached.title, Some("YouTube Video".into()));
    }

    #[test]
    fn cache_all_categories() {
        let cache = ResolveCache::new();

        for (url, category) in [
            ("https://example.com/v.mp4", UrlCategory::DirectMedia),
            ("https://example.com/stream.m3u8", UrlCategory::HlsManifest),
            ("https://example.com/stream.mpd", UrlCategory::DashManifest),
            ("https://youtube.com/watch?v=abc", UrlCategory::WebPage),
            ("http://xyz.onion/v.mp4", UrlCategory::Onion),
        ] {
            let mut result = test_result(url);
            result.category = category;
            cache.insert(url, result);
        }

        assert_eq!(cache.len(), 5);

        // Verify each category roundtrips correctly.
        assert_eq!(
            cache.get("https://example.com/v.mp4").unwrap().category,
            UrlCategory::DirectMedia
        );
        assert_eq!(
            cache.get("https://example.com/stream.m3u8").unwrap().category,
            UrlCategory::HlsManifest
        );
        assert_eq!(
            cache.get("https://example.com/stream.mpd").unwrap().category,
            UrlCategory::DashManifest
        );
        assert_eq!(
            cache.get("https://youtube.com/watch?v=abc").unwrap().category,
            UrlCategory::WebPage
        );
        assert_eq!(cache.get("http://xyz.onion/v.mp4").unwrap().category, UrlCategory::Onion);
    }

    #[test]
    fn cache_clear() {
        let cache = ResolveCache::new();
        cache.insert("url1", test_result("url1"));
        cache.insert("url2", test_result("url2"));
        assert_eq!(cache.len(), 2);

        cache.clear();

        assert!(cache.is_empty());
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn cache_default() {
        let cache = ResolveCache::default();
        assert!(cache.is_empty());
    }

    #[test]
    fn cache_clear_then_reinsert() {
        let cache = ResolveCache::new();
        cache.insert("url1", test_result("url1"));
        cache.clear();
        assert!(cache.is_empty());

        // Should be able to insert again after clear.
        cache.insert("url2", test_result("url2"));
        assert_eq!(cache.len(), 1);
        assert!(cache.get("url2").is_some());
    }

    #[test]
    fn cache_size_tracking() {
        let cache = ResolveCache::new();
        assert_eq!(cache.len(), 0);

        for i in 0..5 {
            let url = format!("https://example.com/video{}.mp4", i);
            cache.insert(&url, test_result(&url));
        }
        assert_eq!(cache.len(), 5);
    }

    #[test]
    fn cache_evict_expired() {
        let cache = ResolveCache::with_ttl(Duration::from_secs(1));

        // Insert an entry.
        cache.insert(
            "https://example.com/video1.mp4",
            test_result("https://example.com/video1.mp4"),
        );
        assert_eq!(cache.len(), 1);

        // Wait well beyond the TTL for it to expire.
        std::thread::sleep(Duration::from_secs(3));

        // Explicitly evict expired entries.
        cache.evict_expired();

        // The entry should be gone.
        assert_eq!(cache.len(), 0, "expired entries should be evicted");
    }

    #[test]
    fn cache_expired_entry_not_returned() {
        let cache = ResolveCache::with_ttl(Duration::from_secs(2));

        cache.insert("https://example.com/video.mp4", test_result("https://example.com/video.mp4"));
        assert!(cache.get("https://example.com/video.mp4").is_some());

        // Wait for the entry to expire.
        std::thread::sleep(Duration::from_millis(2500));

        // The entry should not be returned even without explicit eviction.
        assert!(
            cache.get("https://example.com/video.mp4").is_none(),
            "expired entry should not be returned"
        );
    }

    #[test]
    fn cache_multiple_entries_independent_expiry() {
        let cache = ResolveCache::with_ttl(Duration::from_secs(3));

        let url1 = "https://example.com/video1.mp4";
        cache.insert(url1, test_result(url1));

        // Wait a bit, then insert a second entry.
        std::thread::sleep(Duration::from_secs(2));
        let url2 = "https://example.com/video2.mp4";
        cache.insert(url2, test_result(url2));

        // Wait long enough for url1 to expire but url2 is still valid.
        std::thread::sleep(Duration::from_secs(2));

        assert!(cache.get(url1).is_none(), "url1 should have expired");
        assert!(cache.get(url2).is_some(), "url2 should still be valid");
    }

    #[test]
    fn cache_subtitle_tracks_roundtrip() {
        let cache = ResolveCache::new();
        let url = "https://example.com/video.mp4";
        let mut result = test_result(url);
        result.subtitle_tracks = vec!["en".into(), "es".into(), "fr".into(), "de".into()];
        cache.insert(url, result);

        let cached = cache.get(url).unwrap();
        let mut tracks = cached.subtitle_tracks.clone();
        tracks.sort();
        assert_eq!(tracks, vec!["de", "en", "es", "fr"]);
    }

    #[test]
    fn cache_empty_subtitle_tracks() {
        let cache = ResolveCache::new();
        let url = "https://example.com/video.mp4";
        let mut result = test_result(url);
        result.subtitle_tracks = vec![];
        cache.insert(url, result);

        let cached = cache.get(url).unwrap();
        assert!(cached.subtitle_tracks.is_empty());
    }

    #[test]
    fn cache_none_fields_roundtrip() {
        let cache = ResolveCache::new();
        let url = "https://example.com/plain";
        let result = ResolveResult {
            source_url: url.to_owned(),
            direct_url: url.to_owned(),
            audio_url: None,
            category: UrlCategory::DirectMedia,
            mime_type: None,
            content_length: None,
            used_tor: false,
            title: None,
            duration: None,
            thumbnail: None,
            vcodec: None,
            acodec: None,
            width: None,
            height: None,
            subtitle_tracks: vec![],
            cookies: vec![],
        };
        cache.insert(url, result);

        let cached = cache.get(url).unwrap();
        assert!(cached.mime_type.is_none());
        assert!(cached.content_length.is_none());
        assert!(cached.title.is_none());
        assert!(cached.duration.is_none());
        assert!(cached.thumbnail.is_none());
        assert!(cached.vcodec.is_none());
        assert!(cached.acodec.is_none());
        assert!(cached.width.is_none());
        assert!(cached.height.is_none());
        assert!(cached.subtitle_tracks.is_empty());
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
        drop(cache); // Just verifying construction works without panic.
    }

    #[test]
    fn cache_many_entries() {
        let cache = ResolveCache::new();

        // Insert 50 entries.
        for i in 0..50 {
            let url = format!("https://example.com/video{}.mp4", i);
            cache.insert(&url, test_result(&url));
        }
        assert_eq!(cache.len(), 50);

        // Verify all entries are retrievable.
        for i in 0..50 {
            let url = format!("https://example.com/video{}.mp4", i);
            assert!(cache.get(&url).is_some(), "entry {} should be in cache", i);
        }
    }

    #[test]
    fn cache_concurrent_inserts() {
        use std::sync::Arc;
        use std::thread;

        let cache = Arc::new(ResolveCache::new());
        let mut handles = vec![];

        for i in 0..10 {
            let cache = Arc::clone(&cache);
            handles.push(thread::spawn(move || {
                let url = format!("https://example.com/video{}.mp4", i);
                cache.insert(&url, test_result(&url));
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }

        assert_eq!(cache.len(), 10);
    }
}
