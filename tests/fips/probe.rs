// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Red Hat, Inc.

//! Raw TLS `ClientHello` records, for probes no real TLS stack would send:
//! a client that offers exactly the algorithms the test names and nothing
//! else. Ported from praxis-ai's test harness (`tls_probe`), minus the rogue
//! server: extproc's own TLS surface is its OpenSSL listener, so only the
//! client-side probes apply, and every hello here also offers h2 through
//! ALPN because the listener refuses anything else.
//!
//! Nothing here does cryptography. A probe only needs to see how far the
//! listener gets: a `ServerHello` means the offer was acceptable, an alert
//! or a close means it was refused. That is enough to prove what the
//! listener will and will not negotiate, on any host and in either FIPS
//! branch.
//!
//! Wire formats: RFC 8446 (TLS 1.3) section 4, RFC 5246 (TLS 1.2) section
//! 7.4, RFC 7627 (Extended Master Secret), RFC 8422 (ECC cipher suites),
//! RFC 7301 (ALPN).

// The wire vocabulary is ported whole so probes read like the RFCs and the
// praxis-ai original; not every code is asserted on in every build.
#![allow(dead_code, reason = "ported wire vocabulary kept whole")]

use std::{
    io::{Read as _, Write as _},
    net::TcpStream,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

// -----------------------------------------------------------------------------
// Constants
// -----------------------------------------------------------------------------

/// TLS record content types.
const CONTENT_HANDSHAKE: u8 = 22;
/// The alert content type.
const CONTENT_ALERT: u8 = 21;

/// Handshake message types.
const HANDSHAKE_CLIENT_HELLO: u8 = 1;
/// The `ServerHello` handshake type.
const HANDSHAKE_SERVER_HELLO: u8 = 2;

/// Extension types.
const EXT_SERVER_NAME: u16 = 0;
/// `supported_groups` (RFC 8422, RFC 8446 section 4.2.7).
const EXT_SUPPORTED_GROUPS: u16 = 10;
/// `ec_point_formats` (RFC 8422 section 5.1.2).
const EXT_EC_POINT_FORMATS: u16 = 11;
/// `signature_algorithms` (RFC 8446 section 4.2.3).
const EXT_SIGNATURE_ALGORITHMS: u16 = 13;
/// `application_layer_protocol_negotiation` (RFC 7301).
const EXT_ALPN: u16 = 16;
/// `extended_master_secret` (RFC 7627).
const EXT_EXTENDED_MASTER_SECRET: u16 = 23;
/// `supported_versions` (RFC 8446 section 4.2.1).
const EXT_SUPPORTED_VERSIONS: u16 = 43;
/// `key_share` (RFC 8446 section 4.2.8).
const EXT_KEY_SHARE: u16 = 51;

/// Protocol version codes.
pub(crate) const TLS12: u16 = 0x0303;
/// TLS 1.3 as `supported_versions` names it.
pub(crate) const TLS13: u16 = 0x0304;

/// How long a probe waits for the peer's first record.
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);

// -----------------------------------------------------------------------------
// Algorithm Codes
// -----------------------------------------------------------------------------

/// Cipher suite codes (IANA "TLS Cipher Suites").
pub(crate) mod suites {
    /// `TLS_AES_128_GCM_SHA256`.
    pub(crate) const TLS13_AES_128_GCM_SHA256: u16 = 0x1301;
    /// `TLS_AES_256_GCM_SHA384`.
    pub(crate) const TLS13_AES_256_GCM_SHA384: u16 = 0x1302;
    /// `TLS_CHACHA20_POLY1305_SHA256`.
    pub(crate) const TLS13_CHACHA20_POLY1305_SHA256: u16 = 0x1303;
    /// `TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256`.
    pub(crate) const ECDHE_ECDSA_AES_128_GCM_SHA256: u16 = 0xC02B;
    /// `TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384`.
    pub(crate) const ECDHE_ECDSA_AES_256_GCM_SHA384: u16 = 0xC02C;
    /// `TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256`.
    pub(crate) const ECDHE_RSA_AES_128_GCM_SHA256: u16 = 0xC02F;
    /// `TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384`.
    pub(crate) const ECDHE_RSA_AES_256_GCM_SHA384: u16 = 0xC030;
    /// `TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256`.
    pub(crate) const ECDHE_ECDSA_CHACHA20_POLY1305_SHA256: u16 = 0xCCA9;
    /// `TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256`.
    pub(crate) const ECDHE_RSA_CHACHA20_POLY1305_SHA256: u16 = 0xCCA8;

    /// Every ChaCha20-Poly1305 suite. Not FIPS approved at any module
    /// version, so a FIPS deployment never offers or accepts one.
    pub(crate) const CHACHA20: &[u16] = &[
        TLS13_CHACHA20_POLY1305_SHA256,
        ECDHE_ECDSA_CHACHA20_POLY1305_SHA256,
        ECDHE_RSA_CHACHA20_POLY1305_SHA256,
    ];

    /// The AES-GCM suites, TLS 1.3 and 1.2: what a FIPS deployment
    /// negotiates.
    pub(crate) const AES_GCM: &[u16] = &[
        TLS13_AES_128_GCM_SHA256,
        TLS13_AES_256_GCM_SHA384,
        ECDHE_ECDSA_AES_128_GCM_SHA256,
        ECDHE_ECDSA_AES_256_GCM_SHA384,
        ECDHE_RSA_AES_128_GCM_SHA256,
        ECDHE_RSA_AES_256_GCM_SHA384,
    ];

    /// Whether `suite` uses ChaCha20-Poly1305.
    pub(crate) fn is_chacha20(suite: u16) -> bool {
        CHACHA20.contains(&suite)
    }
}

/// Named group codes (IANA "TLS Supported Groups").
pub(crate) mod groups {
    /// `secp256r1` (NIST P-256).
    pub(crate) const SECP256R1: u16 = 0x0017;
    /// `secp384r1` (NIST P-384).
    pub(crate) const SECP384R1: u16 = 0x0018;
    /// `x25519`.
    pub(crate) const X25519: u16 = 0x001D;

    /// The NIST curves: approved key exchange under FIPS 140-3.
    pub(crate) const NIST: &[u16] = &[SECP256R1, SECP384R1];

    /// A key share for `group` that a server will accept as far as the
    /// handshake needs to go to answer: a real public point for the NIST
    /// curves (a server completes the key exchange before it writes its
    /// `ServerHello`, and a random point is off the curve), and random
    /// bytes of the right length for X25519, where every 32-byte string is
    /// a valid public key.
    ///
    /// # Panics
    ///
    /// Panics when OpenSSL cannot generate an EC key, which in approved
    /// mode it can for both NIST curves.
    pub(crate) fn key_share(group: u16) -> Vec<u8> {
        use openssl::{
            bn::BigNumContext,
            ec::{EcGroup, EcKey, PointConversionForm},
            nid::Nid,
        };
        let curve = match group {
            SECP256R1 => Nid::X9_62_PRIME256V1,
            SECP384R1 => Nid::SECP384R1,
            _ => return super::random(32),
        };
        let ec_group = EcGroup::from_curve_name(curve).expect("named curve");
        let key = EcKey::generate(&ec_group).expect("EC key generation");
        let mut ctx = BigNumContext::new().expect("bignum context");
        key.public_key()
            .to_bytes(&ec_group, PointConversionForm::UNCOMPRESSED, &mut ctx)
            .expect("uncompressed point")
    }
}

/// Signature scheme codes, from the IANA `SignatureScheme` registry.
pub(crate) mod sigalgs {
    /// `ecdsa_secp256r1_sha256`.
    pub(crate) const ECDSA_SECP256R1_SHA256: u16 = 0x0403;
    /// `ecdsa_secp384r1_sha384`.
    pub(crate) const ECDSA_SECP384R1_SHA384: u16 = 0x0503;
    /// `rsa_pss_rsae_sha256`.
    pub(crate) const RSA_PSS_RSAE_SHA256: u16 = 0x0804;
    /// `rsa_pss_rsae_sha384`.
    pub(crate) const RSA_PSS_RSAE_SHA384: u16 = 0x0805;
    /// `rsa_pkcs1_sha256`.
    pub(crate) const RSA_PKCS1_SHA256: u16 = 0x0401;

    /// The schemes a FIPS deployment can sign and verify with.
    pub(crate) const APPROVED: &[u16] = &[
        ECDSA_SECP256R1_SHA256,
        ECDSA_SECP384R1_SHA384,
        RSA_PSS_RSAE_SHA256,
        RSA_PSS_RSAE_SHA384,
        RSA_PKCS1_SHA256,
    ];
}

// -----------------------------------------------------------------------------
// ClientHello Builder
// -----------------------------------------------------------------------------

/// A `ClientHello` that offers exactly what the test says.
#[derive(Debug, Clone)]
pub(crate) struct ClientHello {
    /// The versions offered through `supported_versions`; empty means a
    /// TLS 1.2-only hello with no such extension, as a pre-1.3 client sends.
    pub(crate) versions: Vec<u16>,
    /// Cipher suites, in preference order.
    pub(crate) cipher_suites: Vec<u16>,
    /// Named groups, in preference order. A key share is sent for the first
    /// one when TLS 1.3 is offered.
    pub(crate) groups: Vec<u16>,
    /// Signature schemes.
    pub(crate) signature_algorithms: Vec<u16>,
    /// Whether to offer the Extended Master Secret extension.
    pub(crate) extended_master_secret: bool,
    /// The SNI host name, if any.
    pub(crate) server_name: Option<String>,
}

impl ClientHello {
    /// What a modern, FIPS-acceptable client offers: TLS 1.3 and 1.2, the
    /// AES-GCM suites, the NIST curves, approved signature schemes, EMS.
    pub(crate) fn approved(server_name: &str) -> Self {
        Self {
            versions: vec![TLS13, TLS12],
            cipher_suites: suites::AES_GCM.to_vec(),
            groups: groups::NIST.to_vec(),
            signature_algorithms: sigalgs::APPROVED.to_vec(),
            extended_master_secret: true,
            server_name: Some(server_name.to_owned()),
        }
    }

    /// A TLS 1.2-only hello with the AES-GCM suites and the NIST curves, as
    /// a pre-1.3 client sends it (no `supported_versions`), with or without
    /// EMS.
    pub(crate) fn tls12(server_name: &str, extended_master_secret: bool) -> Self {
        Self {
            versions: Vec::new(),
            cipher_suites: vec![
                suites::ECDHE_ECDSA_AES_128_GCM_SHA256,
                suites::ECDHE_ECDSA_AES_256_GCM_SHA384,
                suites::ECDHE_RSA_AES_128_GCM_SHA256,
                suites::ECDHE_RSA_AES_256_GCM_SHA384,
            ],
            groups: groups::NIST.to_vec(),
            signature_algorithms: sigalgs::APPROVED.to_vec(),
            extended_master_secret,
            server_name: Some(server_name.to_owned()),
        }
    }

    /// Serialize as one TLS record.
    ///
    /// # Panics
    ///
    /// Panics when a field is too long for its wire length prefix.
    pub(crate) fn to_record(&self) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&TLS12.to_be_bytes());
        body.extend_from_slice(&random(32));
        // legacy_session_id: an empty one is valid; real 1.3 clients send a
        // random 32-byte id in compatibility mode.
        body.push(0);
        body.extend_from_slice(&u16_vec(&self.cipher_suites));
        body.extend_from_slice(&[1, 0]); // compression methods: null only
        let extensions = self.extensions();
        push_u16(&mut body, u16::try_from(extensions.len()).expect("extensions length"));
        body.extend_from_slice(&extensions);
        handshake_record(HANDSHAKE_CLIENT_HELLO, &body)
    }

    /// The extensions block.
    fn extensions(&self) -> Vec<u8> {
        let mut extensions = Vec::new();
        if let Some(name) = &self.server_name {
            push_extension(&mut extensions, EXT_SERVER_NAME, &server_name_extension(name));
        }
        push_extension(&mut extensions, EXT_SUPPORTED_GROUPS, &u16_vec(&self.groups));
        push_extension(&mut extensions, EXT_EC_POINT_FORMATS, &[1, 0]); // uncompressed
        push_extension(
            &mut extensions,
            EXT_SIGNATURE_ALGORITHMS,
            &u16_vec(&self.signature_algorithms),
        );
        // The ExtProc listener selects h2 or refuses, so every probe offers
        // it; the refusals under test are the cryptographic ones.
        push_extension(&mut extensions, EXT_ALPN, &alpn_extension(b"h2"));
        if self.extended_master_secret {
            push_extension(&mut extensions, EXT_EXTENDED_MASTER_SECRET, &[]);
        }
        if !self.versions.is_empty() {
            let mut ext = Vec::new();
            push_u8_len_u16_vec(&mut ext, &self.versions);
            push_extension(&mut extensions, EXT_SUPPORTED_VERSIONS, &ext);
        }
        if self.versions.contains(&TLS13)
            && let Some(&group) = self.groups.first()
        {
            push_extension(&mut extensions, EXT_KEY_SHARE, &key_share_extension(group));
        }
        extensions
    }
}

/// The body of a `server_name` extension naming one host.
fn server_name_extension(name: &str) -> Vec<u8> {
    let mut list = vec![0]; // host_name
    push_u16(&mut list, u16::try_from(name.len()).expect("host name length"));
    list.extend_from_slice(name.as_bytes());
    let mut ext = Vec::new();
    push_u16(&mut ext, u16::try_from(list.len()).expect("server_name list length"));
    ext.extend_from_slice(&list);
    ext
}

/// The body of an ALPN extension offering one protocol.
fn alpn_extension(protocol: &[u8]) -> Vec<u8> {
    let mut list = vec![u8::try_from(protocol.len()).expect("protocol length")];
    list.extend_from_slice(protocol);
    let mut ext = Vec::new();
    push_u16(&mut ext, u16::try_from(list.len()).expect("ALPN list length"));
    ext.extend_from_slice(&list);
    ext
}

/// The body of a `key_share` extension with one share for `group`; see
/// [`groups::key_share`] for what makes it acceptable.
fn key_share_extension(group: u16) -> Vec<u8> {
    let share = groups::key_share(group);
    let mut entry = Vec::new();
    push_u16(&mut entry, group);
    push_u16(&mut entry, u16::try_from(share.len()).expect("key share length"));
    entry.extend_from_slice(&share);
    let mut ext = Vec::new();
    push_u16(&mut ext, u16::try_from(entry.len()).expect("key_share length"));
    ext.extend_from_slice(&entry);
    ext
}

// -----------------------------------------------------------------------------
// Client Probe
// -----------------------------------------------------------------------------

/// The peer's first record in reply to a `ClientHello`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Reply {
    /// A `ServerHello`: the offer was acceptable. `version` is the selected
    /// version (`supported_versions` in TLS 1.3, the legacy field
    /// otherwise).
    ServerHello {
        /// The negotiated protocol version.
        version: u16,
        /// The selected cipher suite.
        cipher_suite: u16,
    },
    /// A fatal alert: the offer was refused.
    Alert {
        /// The alert description.
        description: u8,
    },
    /// The peer closed the connection without a record.
    Closed,
}

impl Reply {
    /// Whether the peer refused the offer, by alert or by closing.
    pub(crate) fn refused(&self) -> bool {
        !matches!(self, Self::ServerHello { .. })
    }
}

/// Send `hello` to `addr` and read the first record back.
///
/// # Panics
///
/// Panics when the connection cannot be made or the peer sends something
/// that is neither a `ServerHello` nor an alert within the timeout.
pub(crate) fn probe(addr: &str, hello: &ClientHello) -> Reply {
    let mut stream = TcpStream::connect(addr).expect("connect to the peer");
    stream
        .set_read_timeout(Some(RESPONSE_TIMEOUT))
        .expect("set read timeout");
    stream.write_all(&hello.to_record()).expect("send ClientHello");
    match read_record(&mut stream) {
        None => Reply::Closed,
        Some((CONTENT_ALERT, payload)) => Reply::Alert {
            description: payload.get(1).copied().expect("alert description"),
        },
        Some((CONTENT_HANDSHAKE, payload)) => parse_server_hello(&payload),
        Some((content_type, _)) => panic!("unexpected record type {content_type} in reply to ClientHello"),
    }
}

/// Decode the version and cipher suite out of a `ServerHello`.
fn parse_server_hello(payload: &[u8]) -> Reply {
    assert_eq!(
        payload.first().copied(),
        Some(HANDSHAKE_SERVER_HELLO),
        "expected a ServerHello handshake message"
    );
    let mut cursor = Cursor::new(&payload[4..]);
    let legacy_version = cursor.u16();
    cursor.skip(32);
    let session_id_len = usize::from(cursor.u8());
    cursor.skip(session_id_len);
    let cipher_suite = cursor.u16();
    cursor.skip(1); // compression
    let mut version = legacy_version;
    if cursor.remaining() >= 2 {
        let extensions_len = usize::from(cursor.u16());
        let mut extensions = Cursor::new(cursor.take(extensions_len));
        while extensions.remaining() >= 4 {
            let ext_type = extensions.u16();
            let ext_len = usize::from(extensions.u16());
            let data = extensions.take(ext_len);
            if ext_type == EXT_SUPPORTED_VERSIONS && data.len() == 2 {
                version = u16::from_be_bytes([data[0], data[1]]);
            }
        }
    }
    Reply::ServerHello { version, cipher_suite }
}

// -----------------------------------------------------------------------------
// Record I/O
// -----------------------------------------------------------------------------

/// Read one TLS record: `(content type, payload)`, or `None` at end of
/// stream or timeout before any record.
fn read_record(stream: &mut TcpStream) -> Option<(u8, Vec<u8>)> {
    let mut header = [0_u8; 5];
    stream.read_exact(&mut header).ok()?;
    let len = usize::from(u16::from_be_bytes([header[3], header[4]]));
    let mut payload = vec![0_u8; len];
    stream.read_exact(&mut payload).ok()?;
    Some((header[0], payload))
}

/// Wrap a handshake message body in a handshake message and a record.
fn handshake_record(message_type: u8, body: &[u8]) -> Vec<u8> {
    let mut message = vec![message_type];
    let len = u32::try_from(body.len()).expect("handshake length");
    message.extend_from_slice(&len.to_be_bytes()[1..]);
    message.extend_from_slice(body);
    let mut record = vec![CONTENT_HANDSHAKE, 0x03, 0x03];
    push_u16(&mut record, u16::try_from(message.len()).expect("record length"));
    record.extend_from_slice(&message);
    record
}

/// Append a big-endian `u16`.
fn push_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_be_bytes());
}

/// A `u16`-length-prefixed vector of `u16`.
fn u16_vec(values: &[u16]) -> Vec<u8> {
    let mut out = Vec::new();
    push_u16(&mut out, u16::try_from(values.len() * 2).expect("vector length"));
    for value in values {
        push_u16(&mut out, *value);
    }
    out
}

/// Append a `u8`-length-prefixed vector of `u16` (`supported_versions`).
fn push_u8_len_u16_vec(out: &mut Vec<u8>, values: &[u16]) {
    out.push(u8::try_from(values.len() * 2).expect("vector length"));
    for value in values {
        push_u16(out, *value);
    }
}

/// Append one extension.
fn push_extension(out: &mut Vec<u8>, ext_type: u16, data: &[u8]) {
    push_u16(out, ext_type);
    push_u16(out, u16::try_from(data.len()).expect("extension length"));
    out.extend_from_slice(data);
}

/// Bytes that look random enough for a hello; the probe never derives keys
/// from them, so a cheap generator (xorshift64 seeded from the clock) is
/// fine.
fn random(len: usize) -> Vec<u8> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let mut state = u64::try_from(nanos & u128::from(u64::MAX)).unwrap_or(0x9E37_79B9_7F4A_7C15) | 1;
    std::iter::repeat_with(|| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        u8::try_from(state >> 56).unwrap_or(0)
    })
    .take(len)
    .collect()
}

/// A bounds-checked reader over a byte slice.
struct Cursor<'bytes> {
    /// The bytes.
    bytes: &'bytes [u8],
    /// The read position.
    pos: usize,
}

impl<'bytes> Cursor<'bytes> {
    /// Start at the beginning of `bytes`.
    fn new(bytes: &'bytes [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    /// Bytes left.
    fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.pos)
    }

    /// Read one byte.
    fn u8(&mut self) -> u8 {
        let value = self.bytes[self.pos];
        self.pos += 1;
        value
    }

    /// Read a big-endian `u16`.
    fn u16(&mut self) -> u16 {
        u16::from_be_bytes([self.u8(), self.u8()])
    }

    /// Skip `n` bytes.
    fn skip(&mut self, n: usize) {
        self.pos += n;
    }

    /// Take the next `n` bytes.
    fn take(&mut self, n: usize) -> &'bytes [u8] {
        let slice = &self.bytes[self.pos..self.pos + n];
        self.pos += n;
        slice
    }
}

// -----------------------------------------------------------------------------
// Self-Tests
// -----------------------------------------------------------------------------

#[test]
fn a_crafted_server_hello_parses_back() {
    // A minimal TLS 1.2 ServerHello, built with the same wire helpers.
    let mut body = Vec::new();
    body.extend_from_slice(&TLS12.to_be_bytes());
    body.extend_from_slice(&random(32));
    body.push(0); // session id: none
    push_u16(&mut body, suites::ECDHE_ECDSA_AES_128_GCM_SHA256);
    body.push(0); // compression: null
    push_u16(&mut body, 0); // no extensions
    let record = handshake_record(HANDSHAKE_SERVER_HELLO, &body);
    let reply = parse_server_hello(&record[5..]);
    assert_eq!(
        reply,
        Reply::ServerHello {
            version: TLS12,
            cipher_suite: suites::ECDHE_ECDSA_AES_128_GCM_SHA256
        }
    );
}

#[test]
fn nist_key_shares_are_real_points() {
    let p256 = groups::key_share(groups::SECP256R1);
    assert_eq!(p256.len(), 65);
    assert_eq!(p256.first(), Some(&4), "uncompressed point");
    assert_eq!(groups::key_share(groups::SECP384R1).len(), 97);
    assert_eq!(groups::key_share(groups::X25519).len(), 32);
}
