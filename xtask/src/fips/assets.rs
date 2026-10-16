// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Praxis Contributors

//! Data the FIPS tasks carry with them, compiled in from `xtask/assets/fips/`.

/// OpenSSL configuration that activates the FIPS provider for a single
/// process. Test infrastructure only: on a FIPS-enabled host the provider
/// activates by itself, and an application must never do this.
pub(crate) const FIPS_PROVIDER_CNF: &str = include_str!("../../assets/fips/fips-provider.cnf");

/// The builds of Red Hat's OpenSSL FIPS provider module, by the version
/// string the module reports, with their CMVP status and sources;
/// `host-check` grades the module a host or an image carries against it.
pub(crate) const CERTIFIED_MODULES: &str = include_str!("../../assets/fips/certified-modules.json");

/// The OpenSSL symbols the shipped binary may import from the system
/// libcrypto/libssl, each as `name@OPENSSL_version`: every non-deprecated
/// libcrypto/libssl export through OpenSSL 3.5, one per line (blank lines and
/// `#` comments ignored). `binary` fails the report on any imported OpenSSL
/// symbol not on this list, so an import of a deprecated or otherwise unlisted
/// OpenSSL function is caught before it ships.
pub(crate) const OPENSSL_NONDEPRECATED_SYMBOLS: &str =
    include_str!("../../assets/fips/openssl-nondeprecated-symbols.txt");
