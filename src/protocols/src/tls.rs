//! TLS configuration for boGDan HTTPS/WSS servers.
//!
//! Loads PEM certificate and key files and creates a `tokio-rustls`
//! `TlsAcceptor` that both the HTTP and WebSocket servers share.

use anyhow::{Context, Result};
use std::fs::File;
use std::io::BufReader;
use std::sync::Arc;
use tokio_rustls::rustls::pki_types::PrivateKeyDer;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::TlsAcceptor;

/// Load a TLS acceptor from PEM certificate and key files.
///
/// Returns `None` if either path is empty (TLS disabled).
pub fn load_tls_acceptor(cert_path: &str, key_path: &str) -> Result<Option<TlsAcceptor>> {
    if cert_path.is_empty() || key_path.is_empty() {
        return Ok(None);
    }

    let certs = load_certs(cert_path)
        .with_context(|| format!("failed to load TLS cert from {}", cert_path))?;
    let key =
        load_key(key_path).with_context(|| format!("failed to load TLS key from {}", key_path))?;

    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("failed to build TLS server config")?;

    let acceptor = TlsAcceptor::from(Arc::new(config));

    tracing::info!(
        cert = %cert_path,
        key = %key_path,
        "TLS acceptor loaded — HTTPS/WSS enabled"
    );

    Ok(Some(acceptor))
}

/// Load PEM certificates from a file.
fn load_certs(path: &str) -> Result<Vec<tokio_rustls::rustls::pki_types::CertificateDer<'static>>> {
    let file = File::open(path).with_context(|| format!("cannot open cert file: {}", path))?;
    let mut reader = BufReader::new(file);

    let certs: Vec<_> = rustls_pemfile::certs(&mut reader)
        .collect::<Result<Vec<_>, _>>()
        .context("failed to parse PEM certificates")?;

    if certs.is_empty() {
        anyhow::bail!("no certificates found in {}", path);
    }

    Ok(certs)
}

/// Load a PEM private key from a file.
///
/// Supports PKCS#1 (RSA) and PKCS#8 (any algorithm) private keys.
fn load_key(path: &str) -> Result<PrivateKeyDer<'static>> {
    let file = File::open(path).with_context(|| format!("cannot open key file: {}", path))?;
    let mut reader = BufReader::new(file);

    // Try PKCS#8 first (most common for modern certs).
    for key in rustls_pemfile::pkcs8_private_keys(&mut reader) {
        match key {
            Ok(k) => return Ok(PrivateKeyDer::Pkcs8(k)),
            Err(_) => continue,
        }
    }

    // Reset reader and try PKCS#1 (RSA).
    let file = File::open(path).with_context(|| format!("cannot re-open key file: {}", path))?;
    let mut reader = BufReader::new(file);

    for key in rustls_pemfile::rsa_private_keys(&mut reader) {
        match key {
            Ok(k) => return Ok(PrivateKeyDer::Pkcs1(k)),
            Err(_) => continue,
        }
    }

    anyhow::bail!("no private key found in {} (tried PKCS#8 and PKCS#1)", path)
}
