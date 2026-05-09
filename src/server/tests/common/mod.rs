//! Shared test utilities for boGDan integration tests.
//!
//! Provides helper functions for resolving test configuration from
//! environment variables and waiting for servers to become available.

use std::time::{Duration, Instant};

#[allow(dead_code)]

/// Return the Tor SOCKS proxy address used for tests.
///
/// Defaults to `127.0.0.1:9050` but can be overridden with the
/// `BOGDAN_TEST_TOR_SOCKS` environment variable.
pub fn test_tor_socks_addr() -> String {
    std::env::var("BOGDAN_TEST_TOR_SOCKS").unwrap_or_else(|_| "127.0.0.1:9050".into())
}

/// Return the HTTP API address used for tests.
///
/// Defaults to `127.0.0.1:8585` but can be overridden with the
/// `BOGDAN_TEST_HTTP_ADDR` environment variable.
#[allow(dead_code)]
pub fn test_http_addr() -> String {
    std::env::var("BOGDAN_TEST_HTTP_ADDR").unwrap_or_else(|_| "127.0.0.1:8585".into())
}

/// Poll `addr` with TCP connect attempts until the server is up.
///
/// Panics if the server does not become reachable within `timeout_ms`
/// milliseconds.
#[allow(dead_code)]
pub fn wait_for_server(addr: &str, timeout_ms: u64) {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        if std::net::TcpStream::connect(addr).is_ok() {
            return;
        }
        if Instant::now() >= deadline {
            panic!("server at {} did not become reachable within {}ms", addr, timeout_ms);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
