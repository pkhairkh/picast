---
doc: code_review
project: picast
version: 1
phase: code_review
author: agent
created: 2026-07-30T00:00:00Z
updated: 2026-07-30T00:00:00Z
---

# Code Review: `src/resolver/src/cache.rs`

**File:** `src/resolver/src/cache.rs`
**Lines:** 879
**Reviewer:** agent
**Date:** 2026-07-30

## Summary

The resolution cache stores resolved URL results in a SQLite database with WAL mode for concurrent access. Entries expire after a configurable TTL (default 10 minutes). The cache prevents duplicate resolution of the same URL, saving 3–15 seconds of yt-dlp latency on repeated casts. The implementation is well-structured with 27 tests, schema migration support, and atomic get-or-insert. However, there are several issues.

## Key Components Reviewed

| Component | Lines | Purpose |
|-----------|-------|---------|
| `ResolveCache` struct | 35–42 | SQLite connection + TTL |
| `with_path_and_ttl()` | 63–130 | Constructor with schema creation + migration |
| `insert()` | 132–136 | Insert resolution result |
| `get()` | 141–170 | Look up with TTL check |
| `evict_expired()` | 171–187 | Remove expired entries |
| `delete()` | 211–237 | Delete specific entry (for CDN 403 retry) |
| `get_or_insert_with()` | 239+ | Atomic get-or-insert |

## Findings

### Bugs

#### BUG-001: `conn.lock().unwrap()` panics on poisoned mutex
- **Severity:** Medium
- **Location**: Lines 133, 143, 172, 189, 202, 213, and throughout
- **Description**: Every method uses `self.conn.lock().unwrap()` which panics if the mutex is poisoned. If any method panics while holding the lock (e.g., due to a SQLite error in `expect`), all subsequent cache operations will panic.
- **Impact**: A single panic cascades to all cache operations, making the appliance non-functional.
- **Recommendation**: Use `.lock().unwrap_or_else(|e| e.into_inner())` to recover from poison, or switch to `parking_lot::Mutex` (which doesn't poison). Alternatively, use `tokio::sync::Mutex` since the cache is used in async code.

#### BUG-002: `expect()` in constructor panics on database errors
- **Severity:** Medium
- **Location:** Lines 65, 66, 71, 95 (`expect("failed to open cache database")` at line 65, `expect("failed to create in-memory database")` at line 66, `expect("failed to set WAL mode")` at line 71, `expect("failed to create cache table")` at line 95)
- **Description**: The constructor uses `expect()` for database opening, WAL mode setting, and table creation. If any fails (e.g., disk full, permissions issue, corrupt database), the constructor panics, crashing the server.
- **Impact**: A misconfigured or corrupt cache database crashes the appliance at startup.
- **Recommendation**: Return `Result<Self, ResolveError>` from the constructor instead of panicking. Let the caller decide whether to fall back to an in-memory cache or exit.

#### BUG-003: `get()` doesn't trigger cleanup of expired entries
- **Severity:** Low
- **Location**: `get()` (lines 141–170)
- **Description**: The `get()` method checks `resolved_at > cutoff` (TTL check), so expired entries are not returned. But they're not cleaned up either — they remain in the database until `evict_expired()` or `cleanup_stale()` is called. The `insert()` method (line 132) calls `cleanup_stale()` (for entries older than 1 hour), but entries between TTL (10 min) and cleanup age (1 hour) are never actively removed.
- **Impact**: The database grows with expired-but-not-cleaned entries between 10 minutes and 1 hour old. This wastes space but doesn't affect correctness.
- **Recommendation**: Call `evict_expired()` periodically (e.g., every 10 inserts) or on a timer. The current `cleanup_stale()` only removes entries older than 1 hour; the gap between TTL (10 min) and cleanup (1 hour) is filled with dead entries.

#### BUG-004: `cookies` field is not stored in the cache
- **Severity:** Low
- **Location**: `insert()` and `get()` (cookies field missing from schema)
- **Description**: The `resolved_urls` table schema (lines 87–106) doesn't have a `cookies` column. The `ResolveResult` struct has a `cookies: Vec<String>` field, but it's not persisted. When a cached result is returned, `cookies` is empty.
- **Impact**: A cached resolution that originally returned cookies (e.g., from a CDN that requires session cookies) will have empty cookies when served from cache. The CDN may return 403 on the media download because the session cookie is missing.
- **Recommendation**: Add a `cookies` column (TEXT, JSON-encoded) to the schema. Serialize/deserialize the `Vec<String>` as JSON.

### Design Issues

#### DESIGN-001: `std::sync::Mutex` used in async context
- **Severity:** Low
- **Location**: Line 37 (`conn: Mutex<Connection>`)
- **Description**: The cache uses `std::sync::Mutex` which blocks the Tokio runtime thread while held. All methods are synchronous (no `.await` while holding the lock), so this is technically safe — but it's fragile. A future change that adds `.await` while holding the lock will deadlock.
- **Impact**: Low currently, but high risk of future deadlock if the code is modified.
- **Recommendation**: Use `tokio::sync::Mutex` for consistency with the async codebase, or document clearly that the lock must not be held across `.await` points. Alternatively, use `spawn_blocking` for all cache operations.

#### DESIGN-002: No cache size limit
- **Severity:** Low
- **Location**: Throughout (no size limit)
- **Description**: The cache has no maximum size limit. On an always-on appliance, the database grows indefinitely (though `cleanup_stale()` removes entries older than 1 hour). If the user casts many different URLs, the database could grow significantly.
- **Impact**: The database file grows over time, consuming SD card space. Query performance may degrade with very large tables.
- **Recommendation**: Add a maximum entry count (e.g., 1000 entries). When the limit is reached, evict the oldest entries. Alternatively, rely on the 1-hour cleanup age and document the expected database size.

#### DESIGN-003: Schema migration is manual and fragile
- **Severity:** Low
- **Location:** Lines 97–107 (schema migration for `audio_url` column — `ALTER TABLE` at line 104, error handling at lines 105-107)
- **Description**: The constructor manually checks for the `audio_url` column and adds it if missing. This is a manual migration approach that's fragile — each schema change requires a new migration block, and the migrations must be applied in order.
- **Impact**: Adding new columns requires careful migration code. Missing a migration leaves the column absent, causing runtime errors.
- **Recommendation**: Use a proper migration framework (e.g., `refinery` or `rusqlite_migration`) that tracks schema versions and applies migrations in order. Or use `PRAGMA user_version` to track the schema version.

### Security

#### SEC-001: Cache database stores resolved URLs (which may contain tokens)
- **Severity:** Low
- **Location**: `direct_url` column in the schema
- **Description**: The resolved direct media URLs (which may contain CDN tokens like `?token=abc&expires=123`) are stored in the SQLite database in plaintext. If the SD card is removed and read, the cached URLs reveal the user's viewing history and CDN tokens.
- **Impact**: Privacy concern — the cache database on the SD card contains viewing history. (Same concern as session review SEC-002.)
- **Recommendation**: For v1, document this as a known limitation. For v2, encrypt the database (SQLCipher) or store only the URL hash (not the full URL). The `clear()` method should be exposed in the web UI for privacy-conscious users.

#### SEC-002: No validation of cache database path
- **Severity:** Low
- **Location:** Line 65 (`Connection::open(p).expect("failed to open cache database")`)
- **Description**: The cache database path is not validated. If the path is attacker-controlled, SQLite could create a database at an arbitrary location.
- **Impact**: Low — the path comes from the config, which is root-owned. But defense-in-depth.
- **Recommendation**: Validate that the path is within `/var/lib/bogdan/` or the configured runtime directory.

### Missing Tests

#### TEST-001: 27 tests — good coverage, but no concurrency test
- **Severity:** Low
- **Description**: The file has 27 tests, which is good coverage for a cache module. However, there's no test for concurrent access (multiple threads/tasks reading and writing simultaneously).
- **Impact**: Race conditions in the mutex or SQLite WAL mode are untested.
- **Recommendation**: Add a test that spawns multiple tasks, each doing get/insert/delete concurrently, and verify the cache remains consistent.

#### TEST-002: No test for schema migration
- **Severity:** Low
- **Description**: The schema migration (adding `audio_url` column) is not tested. There's no test that opens an old-format database and verifies the migration runs correctly.
- **Impact**: A migration bug could corrupt old databases.
- **Recommendation**: Add a test that creates a database with the old schema (no `audio_url` column), opens it with the new code, and verifies the column is added.

## Positive Observations

1. **27 tests** — excellent test coverage for a cache module.
2. **WAL mode** — enables concurrent read access, important for multi-protocol access.
3. **TTL-based expiry** — entries expire after 10 minutes by default, preventing stale CDN URLs.
4. **`get_or_insert_with`** — atomic get-or-insert prevents TOCTOU races.
5. **Schema migration** — handles old databases that don't have the `audio_url` column (though fragile — see DESIGN-003).
6. **`cleanup_stale()`** — removes entries older than 1 hour, bounding database growth.
7. **`delete()` for CDN 403 retry** — allows the session layer to invalidate specific entries.
8. **Unix epoch timestamps** — portable and reliable time comparison.
9. **Clear doc comments** — each method is documented with its purpose.
10. **`PRAGMA synchronous=NORMAL`** — balances durability and performance (WAL mode makes this safe).

## Recommendations Summary

| Priority | Finding | Effort |
|----------|---------|--------|
| Medium | BUG-001: Replace `lock().unwrap()` with poison recovery | S (1 h) |
| Medium | BUG-002: Return Result from constructor instead of expect() | S (1–2 h) |
| Low | BUG-003: Periodically evict expired entries | S (30 min) |
| Low | BUG-004: Store cookies in the cache schema | S (1 h) |
| Low | DESIGN-001: Use tokio::sync::Mutex or document the invariant | S (1 h) |
| Low | DESIGN-002: Add maximum cache size limit | S (1–2 h) |
| Low | DESIGN-003: Use a migration framework | M (2–3 h) |
| Low | SEC-001: Document plaintext URL storage or encrypt | S (1 h) |
| Low | SEC-002: Validate cache database path | S (30 min) |
| Low | TEST-001: Add concurrency test | S (1–2 h) |
| Low | TEST-002: Add schema migration test | S (1 h) |
