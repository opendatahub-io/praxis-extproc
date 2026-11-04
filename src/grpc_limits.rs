// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Red Hat, Inc.

//! gRPC per-message receive size for the ExtProc tonic server.
//!
//! Envoy delivers each buffered request/response body chunk as one gRPC
//! message. tonic's compiled default (`4 MiB`) rejects multimodal / agent
//! chat bodies with `decoded message length too large`, which Envoy surfaces
//! as an empty HTTP 500. The default here matches Go IPP / llm-d-router
//! operator practice (`32 MiB`) while remaining a `DoS` bound.
//!
//! Override with [`GRPC_MAX_RECV_MSG_SIZE_ENV`] (plain bytes or a Kubernetes
//! quantity such as `64Mi`). This is a **per-message** cap only; cumulative
//! body accumulation across streamed chunks is still governed by
//! `server.max_body_bytes` / `insecure_options.allow_unbounded_body`.

use std::env;

use crate::error::{ExtProcError, Result};

// -----------------------------------------------------------------------------
// Constants
// -----------------------------------------------------------------------------

/// Environment variable that overrides [`DEFAULT_GRPC_MAX_RECV_MSG_SIZE`].
pub const GRPC_MAX_RECV_MSG_SIZE_ENV: &str = "GRPC_MAX_RECV_MSG_SIZE";

/// Default max decoding message size: above tonic's 4 MiB compiled default,
/// aligned with Go `ai-gateway-payload-processing` / RHAI-13005.
pub const DEFAULT_GRPC_MAX_RECV_MSG_SIZE: usize = 32 * 1024 * 1024;

/// tonic / gRPC compiled default receive limit (bytes).
pub const GRPC_COMPILED_MAX_RECV_DEFAULT: usize = 4 * 1024 * 1024;

/// Kubernetes quantity suffixes, longer first so `Ki` wins over `K`.
const SUFFIXES: &[(&str, u128)] = &[
    ("Ki", 1024),
    ("Mi", 1024 * 1024),
    ("Gi", 1024 * 1024 * 1024),
    ("K", 1000),
    ("k", 1000),
    ("M", 1000 * 1000),
    ("G", 1000 * 1000 * 1000),
];

// -----------------------------------------------------------------------------
// Public API
// -----------------------------------------------------------------------------

/// Resolve the ExtProc gRPC max decoding message size from the process env.
///
/// # Errors
///
/// Returns [`ExtProcError::Config`] when [`GRPC_MAX_RECV_MSG_SIZE_ENV`] is set
/// but is not valid Unicode, or is set to a non-empty value that is not a
/// positive integer or Kubernetes quantity.
pub fn resolve_grpc_max_recv_msg_size() -> Result<usize> {
    let raw = match env::var(GRPC_MAX_RECV_MSG_SIZE_ENV) {
        Ok(value) => Some(value),
        Err(env::VarError::NotPresent) => None,
        Err(error) => {
            return Err(ExtProcError::Config(format!("{GRPC_MAX_RECV_MSG_SIZE_ENV}: {error}")));
        },
    };
    resolve_grpc_max_recv_msg_size_from(raw.as_deref())
}

/// Resolve from an optional raw string (tests inject values without touching env).
pub(crate) fn resolve_grpc_max_recv_msg_size_from(raw: Option<&str>) -> Result<usize> {
    let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(DEFAULT_GRPC_MAX_RECV_MSG_SIZE);
    };

    let n = parse_msg_size_bytes(raw)
        .map_err(|e| ExtProcError::Config(format!("{GRPC_MAX_RECV_MSG_SIZE_ENV}={raw:?}: {e}")))?;
    if n == 0 {
        return Err(ExtProcError::Config(format!(
            "{GRPC_MAX_RECV_MSG_SIZE_ENV}={raw:?}: must be a positive size"
        )));
    }
    Ok(n)
}

// -----------------------------------------------------------------------------
// Parsing
// -----------------------------------------------------------------------------

/// Parse a plain byte count or Kubernetes-style quantity into bytes.
fn parse_msg_size_bytes(raw: &str) -> Result<usize, String> {
    // Prefer plain integers so operators can pass exact byte counts from logs
    // (e.g. the 4194304 from "decoded message length too large").
    if let Ok(n) = raw.parse::<usize>() {
        return Ok(n);
    }

    parse_k8s_quantity(raw)
}

/// Parse a subset of Kubernetes resource quantities used for byte sizes.
///
/// Supports binary (`Ki`/`Mi`/`Gi`) and decimal (`k`/`K`/`M`/`G`) suffixes,
/// including fractional numerics (e.g. `1.5Gi`), matching what operators
/// commonly set on `GRPC_MAX_RECV_MSG_SIZE` in the Go IPP image.
fn parse_k8s_quantity(raw: &str) -> Result<usize, String> {
    for &(suffix, mul) in SUFFIXES {
        if let Some(num) = raw.strip_suffix(suffix) {
            let bytes = parse_decimal_quantity(num.trim(), mul, raw)?;
            if bytes == 0 {
                return Err("size must be positive".into());
            }
            return u128_to_usize(bytes);
        }
    }

    Err(format!("parse size: invalid quantity {raw:?}"))
}

/// Parse an optional leading `+`, integer and/or fractional digits, multiply by
/// `mul`, and round fractional byte results up.
fn parse_decimal_quantity(num: &str, mul: u128, raw: &str) -> Result<u128, String> {
    let num = num.strip_prefix('+').unwrap_or(num);
    let (integer, fraction) = split_decimal_parts(num, raw)?;
    let integer = parse_u128_or_zero(integer, raw)?;
    let (scale, fractional) = fractional_digits(fraction, raw)?;
    scale_and_ceil(integer, scale, fractional, mul, raw)
}

/// Split `num` into integer and fractional digit strings.
fn split_decimal_parts<'a>(num: &'a str, raw: &str) -> Result<(&'a str, &'a str), String> {
    let (integer, fraction) = num.split_once('.').unwrap_or((num, ""));
    if (integer.is_empty() && fraction.is_empty())
        || !integer.bytes().all(|b| b.is_ascii_digit())
        || !fraction.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(format!("parse size: invalid quantity {raw:?}"));
    }
    Ok((integer, fraction))
}

/// Parse an integer digit string, treating empty as zero.
fn parse_u128_or_zero(integer: &str, raw: &str) -> Result<u128, String> {
    if integer.is_empty() {
        return Ok(0);
    }
    integer
        .parse::<u128>()
        .map_err(|_err| format!("size {raw:?} overflows"))
}

/// Convert fractional digits into `(scale, value)` where `value / scale` is the fraction.
fn fractional_digits(fraction: &str, raw: &str) -> Result<(u128, u128), String> {
    let mut scale = 1_u128;
    let mut fractional = 0_u128;
    for digit in fraction.bytes() {
        scale = scale.checked_mul(10).ok_or_else(|| format!("size {raw:?} overflows"))?;
        fractional = fractional
            .checked_mul(10)
            .and_then(|value| value.checked_add(u128::from(digit - b'0')))
            .ok_or_else(|| format!("size {raw:?} overflows"))?;
    }
    Ok((scale, fractional))
}

/// Compute `(integer + fractional/scale) * mul`, rounding up to whole bytes.
fn scale_and_ceil(integer: u128, scale: u128, fractional: u128, mul: u128, raw: &str) -> Result<u128, String> {
    let scaled = integer
        .checked_mul(scale)
        .and_then(|value| value.checked_add(fractional))
        .and_then(|value| value.checked_mul(mul))
        .ok_or_else(|| format!("size {raw:?} overflows"))?;
    let bytes = scaled / scale;
    if scaled % scale == 0 {
        Ok(bytes)
    } else {
        bytes.checked_add(1).ok_or_else(|| format!("size {raw:?} overflows"))
    }
}

/// Convert a byte count that fits in `u128` into `usize`.
fn u128_to_usize(v: u128) -> Result<usize, String> {
    usize::try_from(v).map_err(|_err| format!("size {v} overflows usize"))
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests")]
mod tests {
    use super::*;

    #[test]
    fn empty_uses_default() {
        assert_eq!(
            resolve_grpc_max_recv_msg_size_from(None).expect("empty env uses default"),
            DEFAULT_GRPC_MAX_RECV_MSG_SIZE
        );
        assert_eq!(
            resolve_grpc_max_recv_msg_size_from(Some("")).expect("blank env uses default"),
            DEFAULT_GRPC_MAX_RECV_MSG_SIZE
        );
        assert_eq!(
            resolve_grpc_max_recv_msg_size_from(Some("  \t ")).expect("whitespace env uses default"),
            DEFAULT_GRPC_MAX_RECV_MSG_SIZE
        );
    }

    #[test]
    fn plain_bytes_and_quantities() {
        assert_eq!(
            resolve_grpc_max_recv_msg_size_from(Some("67108864")).expect("plain bytes"),
            64 * 1024 * 1024
        );
        assert_eq!(
            resolve_grpc_max_recv_msg_size_from(Some("32Mi")).expect("binary MiB"),
            32 * 1024 * 1024
        );
        assert_eq!(
            resolve_grpc_max_recv_msg_size_from(Some("1Gi")).expect("binary GiB"),
            1024 * 1024 * 1024
        );
        assert_eq!(
            resolve_grpc_max_recv_msg_size_from(Some("64M")).expect("decimal MB"),
            64 * 1000 * 1000
        );
        assert_eq!(
            resolve_grpc_max_recv_msg_size_from(Some("64k")).expect("lowercase decimal k"),
            64 * 1000
        );
        assert_eq!(
            resolve_grpc_max_recv_msg_size_from(Some("64K")).expect("uppercase decimal K"),
            64 * 1000
        );
    }

    #[test]
    fn fractional_and_signed_quantities() {
        assert_eq!(
            resolve_grpc_max_recv_msg_size_from(Some("1.5Gi")).expect("1.5Gi"),
            (3 * 1024 * 1024 * 1024) / 2
        );
        assert_eq!(
            resolve_grpc_max_recv_msg_size_from(Some("+1Mi")).expect("+1Mi"),
            1024 * 1024
        );
        assert_eq!(resolve_grpc_max_recv_msg_size_from(Some(".5Ki")).expect(".5Ki"), 512);
        // 1.1 * 1000 = 1100 exactly after round-up of 1100/1? 1.1k = 1100
        assert_eq!(resolve_grpc_max_recv_msg_size_from(Some("1.1k")).expect("1.1k"), 1100);
    }

    #[test]
    fn rejects_invalid() {
        for raw in ["0", "-1", "lots", "Mi", "-1Mi"] {
            let err = resolve_grpc_max_recv_msg_size_from(Some(raw)).expect_err("invalid");
            assert!(
                matches!(err, ExtProcError::Config(_)),
                "expected Config error for {raw:?}, got {err:?}"
            );
        }
    }

    #[test]
    fn default_above_grpc_compiled() {
        const {
            assert!(DEFAULT_GRPC_MAX_RECV_MSG_SIZE > GRPC_COMPILED_MAX_RECV_DEFAULT);
        }
    }
}
