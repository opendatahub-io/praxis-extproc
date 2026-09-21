// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Red Hat, Inc.

//! Runtime FIPS approved-mode detection.
//!
//! Nothing else asserts that the process runs under the FIPS provider, and the
//! `openssl` crate's compile-time `fips` module is absent on `OpenSSL` 3. This
//! probes the provider in use at runtime.

use openssl::{
    error::ErrorStack,
    hash::{Hasher, MessageDigest},
    provider::Provider,
};
use tracing::{error, info};

// -----------------------------------------------------------------------------
// Detection
// -----------------------------------------------------------------------------

/// Outcome of the startup FIPS assessment.
#[derive(Debug, Clone, Copy)]
pub struct Status {
    /// Whether the active provider enforces FIPS approved-mode.
    pub active: bool,
    /// Whether this build may serve traffic given its FIPS requirement.
    pub serve_ok: bool,
}

/// Detect FIPS approved-mode, log it, and decide whether this build may serve.
///
/// The `fips` cargo feature makes approved-mode a hard requirement, so a FIPS
/// build with an inactive provider reports `serve_ok = false` and an error.
pub fn assess() -> Status {
    let active = active();
    let required = cfg!(feature = "fips");
    let serve_ok = gate_ok(required, active);
    info!(fips = active, required, "FIPS approved-mode detected");
    if !serve_ok {
        error!("built with the fips feature but FIPS approved-mode is not active; refusing to serve");
    }
    Status { active, serve_ok }
}

/// Whether the active `OpenSSL` provider enforces FIPS approved-mode.
///
/// Two signals must agree: a non-approved digest (MD5) over the EVP path is
/// refused, and the FIPS provider loads. MD5 refusal alone can be a hardened
/// non-FIPS policy, so requiring the provider rejects that false positive.
/// Probing the live provider rather than a boot flag also catches a non-FIPS
/// default provider installed under `fips=1`.
fn active() -> bool {
    md5_probe().is_err() && Provider::load(None, "fips").is_ok()
}

/// Run a non-approved MD5 digest as the FIPS discriminator. The output is unused.
fn md5_probe() -> Result<(), ErrorStack> {
    let mut hasher = Hasher::new(MessageDigest::md5())?;
    hasher.update(b"fips-probe")?;
    hasher.finish()?;
    Ok(())
}

/// Whether the server may serve given the FIPS requirement and detected state.
///
/// Fails closed: when FIPS is required but not active, the server must not serve.
const fn gate_ok(require_fips: bool, active: bool) -> bool {
    !require_fips || active
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn require_fips_without_active_fails_closed() {
        assert!(!gate_ok(true, false), "required and inactive must refuse to serve");
        assert!(gate_ok(true, true), "required and active may serve");
        assert!(gate_ok(false, false), "not required may serve regardless");
        assert!(gate_ok(false, true), "not required may serve regardless");
    }

    #[test]
    fn active_probe_runs() {
        // The value is host-dependent, true only under FIPS. Assert only that the
        // probe completes without panicking. Real FIPS behavior is validated on a
        // FIPS-enabled host.
        let _ = active();
    }

    #[test]
    fn assess_returns_consistent_status() {
        let status = assess();

        if cfg!(feature = "fips") && !status.active {
            assert!(!status.serve_ok, "FIPS required but inactive must refuse to serve");
        }

        if !cfg!(feature = "fips") {
            assert!(status.serve_ok, "non-FIPS build must be willing to serve");
        }

        if cfg!(feature = "fips") && status.active {
            assert!(status.serve_ok, "FIPS required and active must serve");
        }
    }
}
