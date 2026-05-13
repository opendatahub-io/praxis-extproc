// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! TLS configuration for the ExtProc gRPC listener.
//!
//! Supports three modes: self-signed (ephemeral cert), provided
//! (cert and key from disk), and plaintext (no TLS).

use serde::Deserialize;
use tonic::transport::{Identity, ServerTlsConfig};
use tracing::info;

// -----------------------------------------------------------------------------
// TlsMode
// -----------------------------------------------------------------------------

/// TLS mode for the gRPC listener.
///
/// ```
/// use praxis_extproc::tls::TlsMode;
///
/// let mode = TlsMode::default();
/// assert!(matches!(mode, TlsMode::None));
/// ```
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TlsMode {
    /// Generate an ephemeral self-signed certificate at startup.
    SelfSigned,

    /// Load certificate and key from the provided file paths.
    Provided,

    /// No TLS (plaintext gRPC).
    #[default]
    None,
}

// -----------------------------------------------------------------------------
// TlsConfig
// -----------------------------------------------------------------------------

/// TLS settings from the configuration file.
///
/// ```
/// use praxis_extproc::tls::TlsConfig;
///
/// let cfg = TlsConfig::default();
/// assert!(matches!(cfg.mode, praxis_extproc::tls::TlsMode::None));
/// ```
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct TlsConfig {
    /// Which TLS mode to use.
    pub mode: TlsMode,

    /// Path to the PEM certificate file (required for `provided` mode).
    pub cert_path: Option<String>,

    /// Path to the PEM private key file (required for `provided` mode).
    pub key_path: Option<String>,
}

// -----------------------------------------------------------------------------
// TLS Setup
// -----------------------------------------------------------------------------

/// Build a [`ServerTlsConfig`] from the TLS settings.
///
/// Returns `None` for plaintext mode. Generates a self-signed cert
/// or loads from disk depending on the mode.
///
/// # Errors
///
/// Returns an error if cert/key files cannot be read or if
/// self-signed certificate generation fails.
///
/// [`ServerTlsConfig`]: tonic::transport::ServerTlsConfig
pub fn build_tls_config(cfg: &TlsConfig) -> crate::error::Result<Option<ServerTlsConfig>> {
    match cfg.mode {
        TlsMode::None => Ok(None),
        TlsMode::SelfSigned => build_self_signed(),
        TlsMode::Provided => build_provided(cfg),
    }
}

/// Generate a self-signed certificate using `rcgen`.
fn build_self_signed() -> crate::error::Result<Option<ServerTlsConfig>> {
    info!("generating self-signed TLS certificate");

    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()])
        .map_err(|e| crate::error::Error::Config(format!("self-signed cert: {e}")))?;

    let cert_pem = cert.cert.pem();
    let key_pem = cert.key_pair.serialize_pem();

    let identity = Identity::from_pem(cert_pem, key_pem);
    let tls_config = ServerTlsConfig::new().identity(identity);

    Ok(Some(tls_config))
}

/// Load certificate and key from disk.
///
/// # Errors
///
/// Returns an error if `cert_path` or `key_path` is missing, or
/// if the files cannot be read.
fn build_provided(cfg: &TlsConfig) -> crate::error::Result<Option<ServerTlsConfig>> {
    let cert_path = cfg
        .cert_path
        .as_deref()
        .ok_or_else(|| crate::error::Error::Config("tls.cert_path required for provided mode".to_owned()))?;

    let key_path = cfg
        .key_path
        .as_deref()
        .ok_or_else(|| crate::error::Error::Config("tls.key_path required for provided mode".to_owned()))?;

    info!(cert = cert_path, key = key_path, "loading TLS certificate");

    let cert_pem =
        std::fs::read(cert_path).map_err(|e| crate::error::Error::Config(format!("read {cert_path}: {e}")))?;

    let key_pem = std::fs::read(key_path).map_err(|e| crate::error::Error::Config(format!("read {key_path}: {e}")))?;

    let identity = Identity::from_pem(cert_pem, key_pem);
    let tls_config = ServerTlsConfig::new().identity(identity);

    Ok(Some(tls_config))
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    clippy::cognitive_complexity,
    clippy::missing_assert_message,
    clippy::needless_raw_strings,
    clippy::needless_raw_string_hashes,
    reason = "tests"
)]
mod tests {
    use super::*;

    #[test]
    fn none_mode_returns_none() {
        let cfg = TlsConfig::default();
        let result = build_tls_config(&cfg).expect("should succeed");
        assert!(result.is_none(), "None mode should return None");
    }

    #[test]
    fn self_signed_mode_returns_some() {
        let cfg = TlsConfig {
            mode: TlsMode::SelfSigned,
            cert_path: None,
            key_path: None,
        };
        let result = build_tls_config(&cfg).expect("should succeed");
        assert!(result.is_some(), "SelfSigned mode should return Some");
    }

    #[test]
    fn provided_mode_missing_cert_path_errors() {
        let cfg = TlsConfig {
            mode: TlsMode::Provided,
            cert_path: None,
            key_path: Some("/tmp/key.pem".to_owned()),
        };
        let err = build_tls_config(&cfg).unwrap_err();
        assert!(err.to_string().contains("cert_path"), "error should mention cert_path");
    }

    #[test]
    fn provided_mode_missing_key_path_errors() {
        let cfg = TlsConfig {
            mode: TlsMode::Provided,
            cert_path: Some("/tmp/cert.pem".to_owned()),
            key_path: None,
        };
        let err = build_tls_config(&cfg).unwrap_err();
        assert!(err.to_string().contains("key_path"), "error should mention key_path");
    }

    #[test]
    fn provided_mode_nonexistent_files_errors() {
        let cfg = TlsConfig {
            mode: TlsMode::Provided,
            cert_path: Some("/nonexistent/cert.pem".to_owned()),
            key_path: Some("/nonexistent/key.pem".to_owned()),
        };
        assert!(build_tls_config(&cfg).is_err(), "nonexistent files should error");
    }

    #[test]
    fn default_tls_mode_is_none() {
        assert!(matches!(TlsMode::default(), TlsMode::None), "default should be None");
    }

    #[test]
    fn tls_config_deserializes_self_signed() {
        let cfg: TlsConfig = serde_yaml::from_str("mode: self_signed").unwrap();
        assert!(
            matches!(cfg.mode, TlsMode::SelfSigned),
            "should deserialize self_signed"
        );
    }

    #[test]
    fn tls_config_deserializes_provided_with_paths() {
        let cfg: TlsConfig = serde_yaml::from_str(
            r#"
mode: provided
cert_path: /etc/tls/cert.pem
key_path: /etc/tls/key.pem
"#,
        )
        .unwrap();
        assert!(matches!(cfg.mode, TlsMode::Provided), "should deserialize provided");
        assert_eq!(
            cfg.cert_path.as_deref(),
            Some("/etc/tls/cert.pem"),
            "cert path should match"
        );
        assert_eq!(
            cfg.key_path.as_deref(),
            Some("/etc/tls/key.pem"),
            "key path should match"
        );
    }
}
