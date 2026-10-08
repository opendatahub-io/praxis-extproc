// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Red Hat, Inc.

//! Request-body SHA-256 oracle for extended k8s e2e qualification.
//!
//! After request-body EOS, hashes the accumulated body and sets a response
//! header. Hop index comes from `BODY_ORACLE_HOP_INDEX` (not response-header
//! counting — response processing order inverts hop numbering).
//!
//! Per-request hasher state lives in [`HttpFilterContext::filter_state`] so it
//! survives ExtProc phase boundaries (`CarriedContext`); request `extensions`
//! are not carried across streamed body chunks.
//!
//! Digests use OpenSSL EVP (`openssl::hash`) so the FIPS scanner does not
//! flag the legacy `openssl::sha::*` API.

use std::sync::Mutex;

use async_trait::async_trait;
use bytes::Bytes;
use openssl::hash::{Hasher, MessageDigest};
use praxis_filter::{
    BodyAccess, EmptyFilterConfig, FilterAction, FilterError, HttpFilter, HttpFilterContext, parse_filter_config,
};
use tracing::debug;

/// Env var that assigns this Deployment's hop index in request-flow order.
pub const HOP_INDEX_ENV: &str = "BODY_ORACLE_HOP_INDEX";

/// Metadata key holding the lowercase hex digest until `on_response`.
const META_DIGEST: &str = "e2e_body_oracle.digest";

/// Incremental SHA-256 through the system OpenSSL EVP digests.
struct Sha256 {
    /// OpenSSL hasher; finalized in [`Sha256::finish`].
    hasher: Hasher,
}

impl Sha256 {
    /// Open a SHA-256 EVP hasher.
    fn new() -> Result<Self, FilterError> {
        Ok(Self {
            hasher: Hasher::new(MessageDigest::sha256())
                .map_err(|error| FilterError::from(format!("e2e_body_oracle: open SHA-256: {error}")))?,
        })
    }

    /// Absorb `bytes` into the running digest.
    fn update(&mut self, bytes: &[u8]) -> Result<(), FilterError> {
        self.hasher
            .update(bytes)
            .map_err(|error| FilterError::from(format!("e2e_body_oracle: update SHA-256: {error}")))
    }

    /// Finalize and return the 32-byte digest.
    fn finish(mut self) -> Result<[u8; 32], FilterError> {
        let digest = self
            .hasher
            .finish()
            .map_err(|error| FilterError::from(format!("e2e_body_oracle: finish SHA-256: {error}")))?;
        let mut out = [0_u8; 32];
        out.copy_from_slice(&digest);
        Ok(out)
    }
}

/// Per-request hasher parked in `filter_state` (carried across phases).
struct BodyOracleState {
    /// Streaming SHA-256; `None` after finalize.
    hasher: Mutex<Option<Sha256>>,
}

/// Filter that publishes `x-qualification-request-sha256[-hop-N]` on the response.
pub struct BodyOracleFilter {
    /// Request-flow hop index from [`HOP_INDEX_ENV`], when set.
    hop_index: Option<u32>,
}

impl BodyOracleFilter {
    /// Registry factory.
    ///
    /// # Errors
    ///
    /// Returns [`FilterError`] if the YAML config is invalid.
    pub fn from_config(config: &serde_yaml::Value) -> Result<Box<dyn HttpFilter>, FilterError> {
        let _: EmptyFilterConfig = parse_filter_config("e2e_body_oracle", config)?;
        let hop_index = std::env::var(HOP_INDEX_ENV)
            .ok()
            .map(|raw| {
                raw.parse::<u32>().map_err(|error| {
                    FilterError::from(format!(
                        "e2e_body_oracle: {HOP_INDEX_ENV}={raw:?} is not a u32: {error}"
                    ))
                })
            })
            .transpose()?;
        Ok(Box::new(Self { hop_index }))
    }

    /// Response header name for this hop (or single-hop form when unset).
    fn header_name(&self) -> String {
        match self.hop_index {
            Some(n) => format!("x-qualification-request-sha256-hop-{n}"),
            None => "x-qualification-request-sha256".to_owned(),
        }
    }
}

/// Lowercase hex encoding of a SHA-256 digest.
///
/// # Panics
///
/// If OpenSSL cannot provide SHA-256, which no build of it lacks.
#[must_use]
#[expect(
    clippy::expect_used,
    reason = "SHA-256 exists in every OpenSSL build, including the FIPS provider"
)]
pub fn hex_sha256(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new().expect("OpenSSL provides SHA-256");
    hasher.update(bytes).expect("OpenSSL updates a SHA-256 context");
    hasher
        .finish()
        .expect("OpenSSL finishes a SHA-256 context")
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Acquire the per-request hasher mutex.
fn lock_hasher(state: &BodyOracleState) -> Result<std::sync::MutexGuard<'_, Option<Sha256>>, FilterError> {
    state
        .hasher
        .lock()
        .map_err(|_poisoned| FilterError::from("e2e_body_oracle: hasher mutex poisoned"))
}

/// Finish the hasher and stash the hex digest in filter metadata.
fn finalize_digest(ctx: &mut HttpFilterContext<'_>) -> Result<(), FilterError> {
    let Some(state) = ctx.get_filter_state::<BodyOracleState>() else {
        // Distinct from empty-body digest so qualification can detect lost filter state.
        ctx.set_metadata(META_DIGEST, "state-missing".to_owned());
        return Ok(());
    };
    let mut guard = lock_hasher(state)?;
    let Some(hasher) = guard.take() else {
        return Ok(());
    };
    let digest = hasher.finish()?.iter().map(|b| format!("{b:02x}")).collect::<String>();
    drop(guard);
    ctx.set_metadata(META_DIGEST, digest);
    Ok(())
}

/// Finalize an empty or pending hasher when no digest is stashed yet.
fn ensure_digest(ctx: &mut HttpFilterContext<'_>) -> Result<(), FilterError> {
    if ctx.get_metadata(META_DIGEST).is_some() {
        return Ok(());
    }
    match ctx.get_filter_state::<BodyOracleState>() {
        Some(state) => {
            let pending = lock_hasher(state)?.is_some();
            if pending {
                finalize_digest(ctx)?;
            }
        },
        None => ctx.set_metadata(META_DIGEST, "state-missing".to_owned()),
    }
    Ok(())
}

/// Write the qualification digest response header.
fn publish_digest_header(
    filter: &BodyOracleFilter,
    ctx: &mut HttpFilterContext<'_>,
) -> Result<FilterAction, FilterError> {
    ensure_digest(ctx)?;
    let Some(digest) = ctx.get_metadata(META_DIGEST).map(str::to_owned) else {
        return Ok(FilterAction::Continue);
    };
    let name = filter.header_name();
    if let Some(response) = ctx.response_header.as_mut() {
        let header_name = http::HeaderName::try_from(name.as_str())
            .map_err(|error| FilterError::from(format!("e2e_body_oracle: invalid header name {name}: {error}")))?;
        let header_value = http::HeaderValue::from_str(&digest)
            .map_err(|error| FilterError::from(format!("e2e_body_oracle: invalid digest value: {error}")))?;
        response.headers.insert(header_name, header_value);
    }
    debug!(
        target: "praxis_extproc::e2e_body_oracle",
        qualification_body_digest = %digest,
        hop_index = ?filter.hop_index,
        "qualification body digest"
    );
    Ok(FilterAction::Continue)
}

#[async_trait]
impl HttpFilter for BodyOracleFilter {
    fn name(&self) -> &'static str {
        "e2e_body_oracle"
    }

    fn request_body_access(&self) -> BodyAccess {
        BodyAccess::ReadOnly
    }

    async fn on_request(&self, ctx: &mut HttpFilterContext<'_>) -> Result<FilterAction, FilterError> {
        ctx.insert_filter_state(BodyOracleState {
            hasher: Mutex::new(Some(Sha256::new()?)),
        });
        Ok(FilterAction::Continue)
    }

    async fn on_request_body(
        &self,
        ctx: &mut HttpFilterContext<'_>,
        body: &mut Option<Bytes>,
        end_of_stream: bool,
    ) -> Result<FilterAction, FilterError> {
        if let Some(chunk) = body.as_ref()
            && let Some(state) = ctx.get_filter_state::<BodyOracleState>()
        {
            let mut guard = lock_hasher(state)?;
            if let Some(hasher) = guard.as_mut() {
                hasher.update(chunk)?;
            }
        }
        if end_of_stream {
            finalize_digest(ctx)?;
        }
        Ok(FilterAction::Continue)
    }

    async fn on_response(&self, ctx: &mut HttpFilterContext<'_>) -> Result<FilterAction, FilterError> {
        publish_digest_header(self, ctx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_sha256_empty_matches_known_vector() {
        assert_eq!(
            hex_sha256(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "empty SHA-256 vector"
        );
    }

    #[test]
    fn hex_sha256_abc_matches_known_vector() {
        assert_eq!(
            hex_sha256(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            "abc SHA-256 vector"
        );
    }

    #[test]
    fn header_name_uses_hop_suffix_when_configured() {
        let filter = BodyOracleFilter { hop_index: Some(2) };
        assert_eq!(
            filter.header_name(),
            "x-qualification-request-sha256-hop-2",
            "hop suffix"
        );
    }

    #[test]
    fn header_name_single_hop_when_env_absent() {
        let filter = BodyOracleFilter { hop_index: None };
        assert_eq!(
            filter.header_name(),
            "x-qualification-request-sha256",
            "single-hop name"
        );
    }
}
