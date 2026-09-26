// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Red Hat, Inc.

//! The crypto provider, and FIPS mode.
//!
//! Every cryptographic primitive this process runs goes through the system
//! OpenSSL. The ExtProc listener speaks TLS through OpenSSL directly
//! (`tokio-openssl`), and everything the Praxis filters do over TLS, such as
//! subrequests, goes through rustls, which performs no cryptography of its own
//! and delegates every primitive to a `CryptoProvider`. This process installs
//! exactly one, the OpenSSL-backed provider, before any filter or connector is
//! built. On a FIPS-enabled Red Hat Enterprise Linux host the library behind
//! both paths is the platform's validated module.
//!
//! FIPS mode itself comes from the host: the kernel flag activates the
//! validated OpenSSL provider and the system crypto policy, and this process
//! never enables a provider on its own. What it does is report the two signals
//! at startup and, when [`REQUIRE_FIPS_ENV`] is set, refuse to serve unless
//! both are present.
//!
//! The provider, the signals, the variable and the messages are
//! [`praxis_tls::provider`]'s, the module this one was copied from while no
//! praxis release carried it; now that one does, this module only adds the
//! [`ExtProcError`] shape and the fail-closed [`require`] helper.

pub use praxis_tls::provider::{REQUIRE_FIPS_ENV, Status, required, status};
use tracing::{debug, info};

use crate::error::ExtProcError;

// -----------------------------------------------------------------------------
// Install
// -----------------------------------------------------------------------------

/// Install the OpenSSL-backed provider as the process-wide default, read the
/// FIPS signals and log them.
///
/// Must run before anything builds a filter registry, a subrequest client or a
/// TLS configuration: rustls has no built-in fallback in this build, so a
/// provider that is not installed here is not installed at all. Idempotent: a
/// provider installed earlier (by a test, say) stays, since the first caller
/// wins.
///
/// # Errors
///
/// Returns [`ExtProcError::Crypto`] when no provider is installed afterwards.
pub fn install() -> Result<Status, ExtProcError> {
    if !praxis_tls::provider::install() {
        debug!("a crypto provider was already installed; keeping it");
    }
    let status = status();
    if !status.installed {
        return Err(ExtProcError::Crypto(format!(
            "failed to install the {} crypto provider",
            praxis_tls::provider::name()
        )));
    }
    info!(
        provider = status.name,
        provider_fips = status.provider_fips,
        kernel_fips = ?status.kernel_fips,
        fips_required = required(),
        "installed rustls crypto provider"
    );
    Ok(status)
}

// -----------------------------------------------------------------------------
// Requirement
// -----------------------------------------------------------------------------

/// Whether FIPS mode is in effect: a provider installed and both signals
/// present ([`Status::unmet`] empty).
#[must_use]
pub fn active(status: &Status) -> bool {
    status.unmet().is_empty()
}

/// Fail closed: when [`REQUIRE_FIPS_ENV`] is set and FIPS mode is not in
/// effect, an error naming every missing signal.
///
/// # Errors
///
/// Returns [`ExtProcError::Crypto`] with the reasons from [`Status::unmet`].
pub fn require(status: &Status) -> Result<(), ExtProcError> {
    require_if(status, required())
}

/// [`require`] with the requirement decided by the caller.
fn require_if(status: &Status, required: bool) -> Result<(), ExtProcError> {
    if !required {
        return Ok(());
    }
    let unmet = status.unmet();
    if unmet.is_empty() {
        return Ok(());
    }
    Err(ExtProcError::Crypto(format!(
        "{REQUIRE_FIPS_ENV} is set but FIPS mode is not in effect: {}",
        unmet.join("; ")
    )))
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests")]
mod tests {
    use super::*;

    /// The status of a FIPS host.
    const FIPS_HOST: Status = Status {
        name: "openssl",
        installed: true,
        provider_fips: true,
        kernel_fips: Some(true),
    };

    #[test]
    fn fips_mode_needs_both_signals() {
        assert!(active(&FIPS_HOST), "provider and kernel both report FIPS");
        assert!(FIPS_HOST.unmet().is_empty(), "nothing is missing on a FIPS host");
        let neither = Status {
            provider_fips: false,
            kernel_fips: Some(false),
            ..FIPS_HOST
        };
        assert!(!active(&neither), "no signal, no FIPS mode");
        assert_eq!(neither.unmet().len(), 2, "both missing signals are named");
    }

    #[test]
    #[expect(clippy::too_many_lines, reason = "one case per missing signal")]
    fn each_missing_signal_is_named() {
        for (status, missing) in [
            (
                Status {
                    provider_fips: false,
                    ..FIPS_HOST
                },
                "OpenSSL provider",
            ),
            (
                Status {
                    installed: false,
                    provider_fips: false,
                    ..FIPS_HOST
                },
                "no crypto provider",
            ),
            (
                Status {
                    kernel_fips: Some(false),
                    ..FIPS_HOST
                },
                "not in FIPS mode",
            ),
            (
                Status {
                    kernel_fips: None,
                    ..FIPS_HOST
                },
                "cannot be read",
            ),
        ] {
            assert!(!active(&status), "one missing signal means no FIPS mode: {status:?}");
            let unmet = status.unmet();
            assert_eq!(unmet.len(), 1, "exactly the missing signal is named: {unmet:?}");
            let reason = unmet.first().copied().expect("one reason");
            assert!(reason.contains(missing), "the reason names the signal: {reason}");
        }
    }

    #[test]
    fn requirement_fails_closed_only_when_set_and_unmet() {
        let off = Status {
            provider_fips: false,
            kernel_fips: Some(false),
            ..FIPS_HOST
        };
        assert!(require_if(&off, false).is_ok(), "not required: serve regardless");
        assert!(require_if(&FIPS_HOST, true).is_ok(), "required and met: serve");
        let err = require_if(&off, true).expect_err("required and unmet: refuse");
        let message = err.to_string();
        assert!(
            message.contains("PRAXIS_REQUIRE_FIPS is set but FIPS mode is not in effect"),
            "the message names the variable and the state: {message}"
        );
        assert!(
            message.contains("provider") && message.contains("kernel"),
            "and every missing signal: {message}"
        );
    }

    #[test]
    fn install_is_idempotent_and_leaves_a_provider_installed() {
        let first = install().expect("install");
        let second = install().expect("a second install keeps the first provider");
        assert_eq!(first, second, "the signals do not change between calls");
        assert!(praxis_tls::provider::installed(), "a provider is installed");
    }
}
