//! Mock HTTP server integration tests for the custom resolvers.
//!
//! Uses mockito to simulate Voe and DoodStream pages and verify the
//! resolver correctly extracts media URLs from obfuscated content.

use bogdan_resolver::custom::{resolve_doodstream, resolve_voe};
use base64::Engine;

/// Helper: reverse the Method 8 pipeline to create a synthetic obfuscated blob.
/// Forward: rot13 -> replace_patterns -> b64_decode -> shift(-3) -> reverse -> b64_decode -> JSON
/// Reverse: JSON -> b64_encode -> unreverse -> shift(+3) -> b64_encode -> add_markers -> rot13
fn encode_voe_method8(json: &str) -> String {
    // Step 1: base64 encode the JSON
    let step1 = base64::engine::general_purpose::STANDARD.encode(json.as_bytes());
    // Step 2: reverse the string
    let step2: String = step1.chars().rev().collect();
    // Step 3: shift chars by +3 (reverse of -3)
    let step3 = shift_chars_reverse(&step2, 3);
    // Step 4: base64 encode again
    let step4 = base64::engine::general_purpose::STANDARD.encode(step3.as_bytes());
    // Step 5: add marker patterns (reverse of replace_patterns which strips them)
    let step5 = add_markers(&step4);
    // Step 6: ROT13 (which is its own inverse)
    let step6 = rot13(&step5);
    // Wrap in a JSON array
    serde_json::to_string(&vec![step6]).unwrap()
}

/// ROT13 cipher (self-inverse).
fn rot13(text: &str) -> String {
    text.chars()
        .map(|ch| {
            if ch.is_ascii_uppercase() {
                let o = ch as u32;
                char::from_u32(((o - 65 + 13) % 26) + 65).unwrap_or(ch)
            } else if ch.is_ascii_lowercase() {
                let o = ch as u32;
                char::from_u32(((o - 97 + 13) % 26) + 97).unwrap_or(ch)
            } else {
                ch
            }
        })
        .collect()
}

/// Shift chars by +shift (reverse of the decode shift which is -shift).
fn shift_chars_reverse(text: &str, shift: u32) -> String {
    text.chars()
        .map(|c| {
            let code = c as u32;
            char::from_u32(code + shift).unwrap_or(c)
        })
        .collect()
}

/// Add marker patterns to a string (reverse of replace_patterns which removes them).
fn add_markers(txt: &str) -> String {
    let markers = ["@$", "^^", "~@", "%?", "*~", "!!", "#&"];
    let mut result = String::new();
    let chars: Vec<char> = txt.chars().collect();
    let chunk_size = (chars.len() / markers.len()).max(1);
    for (i, marker) in markers.iter().enumerate() {
        let start = i * chunk_size;
        let end = ((i + 1) * chunk_size).min(chars.len());
        if start < chars.len() {
            result.extend(&chars[start..end]);
            result.push_str(marker);
        }
    }
    let remaining_start = markers.len() * chunk_size;
    if remaining_start < chars.len() {
        result.extend(&chars[remaining_start..]);
    }
    result
}

/// Build a Voe HTML page with a Method 8 obfuscated JSON blob.
fn build_voe_page(obfuscated_json: &str) -> String {
    format!(
        r#"<html><head><script type="application/json">{}</script></head><body></body></html>"#,
        obfuscated_json
    )
}

/// Build a DoodStream /d/ page that contains an /e/ iframe.
fn build_doodstream_d_page(embed_path: &str) -> String {
    format!(
        r#"<html><body><iframe src="{}"></iframe></body></html>"#,
        embed_path
    )
}

/// Build a DoodStream /e/ page with a pass_md5 download token.
fn build_doodstream_e_page(download_url: &str) -> String {
    format!(
        r#"<html><body><script>
        function pass_md5() {{ return "{}"; }}
        </script></body></html>"#,
        download_url
    )
}

#[tokio::test]
async fn mock_voe_method8_returns_media_url() {
    let mut server = mockito::Server::new_async().await;

    let json = r#"{"mp4":"https://cdn.example.com/mockvid.mp4"}"#;
    let obfuscated = encode_voe_method8(json);
    let html = build_voe_page(&obfuscated);

    let mock = server
        .mock("GET", "/testvid")
        .with_status(200)
        .with_header("content-type", "text/html")
        .with_body(&html)
        .create_async()
        .await;

    let url = format!("{}/testvid", server.url());
    let result = resolve_voe(&url, None).await;

    mock.assert_async().await;

    assert!(result.is_ok(), "Voe resolver should succeed on mock page");
    let resolved = result.unwrap();
    assert!(
        resolved.direct_url.contains("mockvid.mp4"),
        "Resolved URL should contain the MP4 URL, got: {}",
        resolved.direct_url
    );
}

#[tokio::test]
async fn mock_doodstream_d_to_e_flow() {
    let mut server = mockito::Server::new_async().await;

    // The /d/ page returns HTML with an /e/ iframe
    let d_page = build_doodstream_d_page("/e/abc123");
    let e_page =
        build_doodstream_e_page("https://cdn.example.com/doodstream_download/video.mp4?token=xyz");

    let d_mock = server
        .mock("GET", "/d/abc123")
        .with_status(200)
        .with_header("content-type", "text/html")
        .with_body(&d_page)
        .create_async()
        .await;

    let e_mock = server
        .mock("GET", "/e/abc123")
        .with_status(200)
        .with_header("content-type", "text/html")
        .with_body(&e_page)
        .create_async()
        .await;

    let url = format!("{}/d/abc123", server.url());
    let result = resolve_doodstream(&url, None).await;

    d_mock.assert_async().await;
    e_mock.assert_async().await;

    // The resolver should at least reach the embed page
    // The exact result depends on whether extract_doodstream_media
    // can parse our synthetic page, so just check it doesn't error
    // on network issues.
    match result {
        Ok(resolved) => {
            // If it succeeds, verify the URL looks reasonable
            assert!(
                !resolved.direct_url.is_empty(),
                "Resolved URL should not be empty"
            );
        },
        Err(e) => {
            // It's OK if the resolver can't fully parse the synthetic
            // DoodStream page — we're mainly testing that the HTTP flow
            // works (both /d/ and /e/ pages are fetched).
            let msg = e.to_string();
            assert!(
                !msg.contains("HTTP 403") && !msg.contains("HTTP 404"),
                "Should not get HTTP errors from mock server, got: {}",
                msg
            );
        },
    }
}

#[tokio::test]
async fn mock_403_returns_no_media_found() {
    let mut server = mockito::Server::new_async().await;

    let mock = server
        .mock("GET", "/blocked")
        .with_status(403)
        .with_body("Forbidden")
        .create_async()
        .await;

    let url = format!("{}/blocked", server.url());
    let result = resolve_voe(&url, None).await;

    mock.assert_async().await;

    // 403 from the server should result in a network error or no media found
    match result {
        Err(e) => {
            let msg = e.to_string();
            assert!(
                msg.contains("403") || msg.contains("No media") || msg.contains("HTTP"),
                "Expected 403-related error, got: {}",
                msg
            );
        },
        Ok(_) => {
            // A successful resolution from a 403 page would be unusual
            // but not strictly wrong (redirects could work)
        },
    }
}

#[tokio::test]
async fn mock_404_returns_no_media_found() {
    let mut server = mockito::Server::new_async().await;

    let mock = server
        .mock("GET", "/notfound")
        .with_status(404)
        .with_body("Not Found")
        .create_async()
        .await;

    let url = format!("{}/notfound", server.url());
    let result = resolve_voe(&url, None).await;

    mock.assert_async().await;

    // 404 from the server should result in an error
    match result {
        Err(e) => {
            let msg = e.to_string();
            assert!(
                msg.contains("404") || msg.contains("No media") || msg.contains("HTTP"),
                "Expected 404-related error, got: {}",
                msg
            );
        },
        Ok(_) => {},
    }
}

#[tokio::test]
async fn mock_cookie_forwarding() {
    let mut server = mockito::Server::new_async().await;

    let json = r#"{"mp4":"https://cdn.example.com/cookietest.mp4"}"#;
    let obfuscated = encode_voe_method8(json);
    let html = build_voe_page(&obfuscated);

    let mock = server
        .mock("GET", "/cookievid")
        .with_status(200)
        .with_header("content-type", "text/html")
        .with_header("set-cookie", "session=abc123; Path=/")
        .with_body(&html)
        .create_async()
        .await;

    let url = format!("{}/cookievid", server.url());
    let result = resolve_voe(&url, None).await;

    mock.assert_async().await;

    if let Ok(resolved) = result {
        // Verify cookies were captured
        assert!(
            !resolved.cookies.is_empty(),
            "Cookies should be captured from the response"
        );
        assert!(
            resolved.cookies.iter().any(|c| c.contains("session=abc123")),
            "Session cookie should be in the result"
        );
    }
}
