# FIPS 140-3

Praxis ExtProc performs all of its cryptography in the system OpenSSL
library. The ExtProc listener's own TLS goes through OpenSSL directly, and
everything the Praxis filters do over TLS (the subrequests that guardrails,
metering and provider filters make) goes through rustls with the
OpenSSL-backed crypto provider that the server installs at startup, the same
one [Praxis] uses. On a Red Hat Enterprise Linux 9 host in FIPS mode that
library's provider is the validated module (Red Hat Enterprise Linux 9
OpenSSL FIPS Provider, CMVP certificate #4857 at the time of writing; Red Hat
keeps the current list at <https://access.redhat.com/compliance/fips>), so
the server's TLS, hashing and random numbers happen inside a FIPS 140-3
validated boundary. The server itself is not a validated module and never
enables FIPS mode on its own: FIPS mode comes from the host, and the server
reports and, on request, enforces it.

This page is for people deploying the image and for anyone deciding what
goes into it. How the build is checked and how the tooling works is in
[Development](development.md#fips-tooling).

[Praxis]: https://github.com/praxis-proxy/praxis

## The product image is the FIPS build

The `Containerfile` builds the FIPS feature set by default, on
`ubi9/ubi` with Red Hat's `rust-toolset` and OpenSSL, and ships on
`ubi9/ubi-minimal`; both bases are pinned by digest and their Red Hat
signatures are verified before every podman build here. Konflux builds the
same file. There is no separate FIPS tag: the image is the FIPS build.

The FIPS build differs from a plain `cargo build` only in its cargo features.
It leaves out what is known to carry pure-Rust cryptography, so nobody has to
know which features to pick:

| | `cargo build` (default features) | The image (`FIPS_FEATURES` in the Makefile) |
|---|---|---|
| `responses`: the OpenAI Responses filters | yes | yes |
| `responses-store`: the response store the Responses filters keep state in | yes | no: built on sqlx, whose migration checksums use `sha2` |
| `responses-full`: the rest of what Praxis AI offers on that store (the Postgres and SQLite backends, Conversations, context compaction, MCP tools, the file resolver) | yes | no: all of it needs the store |
| `aws-sigv4`: the `aws_sigv4_sign` filter | yes | yes: signs through the system OpenSSL (praxis-ai moved it off `sha2`/`hmac`; the `aws-sigv4` crate is only its test oracle) |
| `policy-engine`: the praxis `policy` filter (the Praxis Policy Engine) | yes | no: its runtime and plugins carry `aws-lc-rs`, `sha2` and `hmac` |

`FIPS_FEATURES` is defined once, in the Makefile; the `Containerfile`'s
`CARGO_FEATURES` default mirrors it.

## What the FIPS build leaves out, in detail

**The Responses store** (`responses-store`). The Responses filters share a
response store registry, installed as a pipeline extension, that
`openai_responses_rehydrate` reads earlier responses from
(`previous_response_id` chaining) and `openai_response_store` writes finished
responses to. Without it, stateless Responses requests work as before, and
a configuration that names those two filters still starts, but a request
that reaches `openai_responses_rehydrate` is rejected with a 500 ("response
store is not available") and `openai_response_store` skips persistence.
What makes it unclean is `sqlx-core`, which depends on `sha2` for its
migration checksums, and Red Hat's scanner fails on the crate's name alone.
Fixing it means either an in-memory store in praxis-ai that does not go
through sqlx, or sqlx making `sha2` optional upstream.

**Everything on the store** (`responses-full`). The Postgres and SQLite
store backends (`sqlx-postgres` adds `md-5`, `hmac`, `sha2` and `hkdf` of
its own for SCRAM authentication), the Conversations API
(`openai_conversations`), context compaction (`openai_responses_compact`),
MCP tool resolution and dispatch (`rmcp`, whose HTTP transport is reqwest
with its `rustls` feature, which compiles in `aws-lc-rs`), and the file
resolver (`openai_file_resolve`, the same reqwest). All of them need the
store, so they go with it; a configuration naming one of those filters is
rejected at startup by the image. No shipped configuration does.

**The `aws_sigv4_sign` filter** (`aws-sigv4`) is in the FIPS build. AWS
Signature V4 request signing for Bedrock-style backends. It used to be
excluded because the `aws-sigv4` crate computes the signature with `sha2`
and `hmac`, both on the denylist; praxis-ai has since moved the signing
onto OpenSSL's EVP APIs (`openssl::hash`, `openssl::sign`), keeping the
`aws-sigv4` crate only as the test oracle the signer is checked against,
so the filter ships.

**The praxis `policy` filter** (`policy-engine`). The Praxis Policy Engine,
which praxis-filter builds from the praxis-policy crates
([praxis-proxy/policy]). Its runtime hashes with `sha2`, its JWT plugin
verifies with `jsonwebtoken` (which compiles in `aws-lc-rs`), and its OAuth
and Valkey plugins use `sha2` and `hmac`. No shipped configuration uses the
filter; a configuration naming it is rejected at startup by the image.
Fixing it means the engine doing its cryptography through OpenSSL in
praxis-policy.

[praxis-proxy/policy]: https://github.com/praxis-proxy/policy

All of these are still on for a default `cargo build`, so a developer's
local build is what it was.

## Host prerequisites

- A RHEL 9 host in FIPS mode (on OpenShift, a cluster installed with FIPS
  enabled): `cat /proc/sys/crypto/fips_enabled` prints `1` and
  `openssl list -providers` lists `fips`. RHEL 9 is the validated operating
  environment; the module that runs inside the container is UBI's own
  `openssl-fips-provider-so`, the host contributes the kernel flag.
- A container runtime that passes the host's FIPS mode into the container,
  as CRI-O and podman on RHEL do. The image then needs no flag, environment
  variable or config: the OpenSSL inside it reads the kernel flag and
  activates the validated provider by itself. On a host that is not in FIPS
  mode the same image runs with OpenSSL's default provider, and the startup
  log says so.

## Startup: what the server reports and enforces

At startup the server installs its one crypto provider and logs what it
found:

```text
installed rustls crypto provider provider="openssl" provider_fips=true kernel_fips=Some(true) fips_required=true
```

- `provider_fips`: whether OpenSSL's default properties select FIPS-approved
  algorithms only (`EVP_default_properties_is_fips_enabled`), which is what
  RHEL's FIPS mode configures.
- `kernel_fips`: `/proc/sys/crypto/fips_enabled`; `None` where the file does
  not exist (a container without `/proc`, a non-Linux host).

The gRPC health service reports the same state under the service name
`fips`: `SERVING` when both signals are present, `NOT_SERVING` otherwise,
independently of the ExtProc service's own readiness.

Set `PRAXIS_REQUIRE_FIPS=1` (also `true`, `yes`, `on`) in production FIPS
deployments. It is a check, never a switch: the server then refuses to serve
unless both signals are present, naming each one that is missing. The
process stays up and inspectable (health `NOT_SERVING`, metrics served) so
the failure shows in the pod rather than in a crash loop, and
`--validate` fails outright. Without the variable the server starts either
way and only logs the status.

```console
podman run --rm -e PRAXIS_REQUIRE_FIPS=1 \
    -v $(pwd)/examples/praxis-extproc.yaml:/etc/praxis/extproc.yaml:ro \
    quay.io/opendatahub/odh-praxis-extproc:odh-stable --validate -c /etc/praxis/extproc.yaml
```

On a host that is not in FIPS mode this exits 1 with

```text
fatal error=crypto: PRAXIS_REQUIRE_FIPS is set but FIPS mode is not in effect: the OpenSSL provider does not report FIPS-approved algorithms (is the fips provider active?); the kernel is not in FIPS mode (/proc/sys/crypto/fips_enabled is 0)
```

## What changes under FIPS mode

- **The listener offers only what the system OpenSSL can perform.** The
  ExtProc listener's TLS (`server.tls` in the configuration) is OpenSSL's
  own `SslAcceptor`; in FIPS mode non-approved algorithms are absent rather
  than failing later, and keys or certificates the module refuses (short RSA
  keys, legacy signature algorithms) are rejected at load or at first use.
  Which algorithms are approved is decided by the validated module and the
  host's crypto policy, not by the server.
- **The filters' TLS goes through the same module.** Subrequest connections
  use rustls with the OpenSSL provider; in FIPS mode TLS 1.2 requires the
  Extended Master Secret extension and the ChaCha20-Poly1305 suites are not
  offered.
- **Random numbers** for TLS come from OpenSSL's DRBG. Non-security
  randomness (request ids) uses ordinary Rust RNGs, which is fine: they
  protect nothing.

## Verifying a deployment

On a developer machine (no FIPS host needed):

```console
make fips-signature-store  # once on Debian/Ubuntu: their podman has no entry for Red Hat's signature store
make fips-check            # build on UBI 9 with Red Hat's toolchain, print the compliance report
make container-release     # the product image
make fips-smoke            # run it once: the example config validates with the system OpenSSL
make fips-scanner          # build Red Hat's scanner (check-payload) at its pinned revision; needs Go and oc
make fips-scan             # run it against the image, warnings fatal
```

On a RHEL 9 host in FIPS mode, the runtime proof (what CI's `fips-host` job
runs on every change):

```console
make fips-host-check     # attest the host and the image's module build (target/fips/host-attestation.*)
make test-fips-host      # the whole test suite as the FIPS build, inside the UBI 9 toolchain image, fail-closed on FIPS mode
make fips-runtime-probe  # run the product image under PRAXIS_REQUIRE_FIPS=1 and probe its TLS listener
```

`fips-host-check` states every fact the module's Security Policy requires of
the host (the kernel flag, `fips=1` on the command line, the `FIPS` crypto
policy, `fips-mode-setup --check`, the module the host's OpenSSL loads) and
of the image (the crypto policy podman propagates into it, the build of
`fips.so` it carries and whether that build is on a CMVP certificate), and
writes the attestation to `target/fips/` to keep with the deployment record.
`test-fips-host` runs the suite with `PRAXIS_FIPS_HOST=1`, so a green run
cannot have happened outside FIPS mode: the FIPS behavior tests
(`tests/fips/`) then insist on their approved-mode branches, in which the
OpenSSL listener refuses ChaCha20-only, X25519-only and non-EMS TLS 1.2
clients and negotiates AES-GCM on the NIST curves. `fips-runtime-probe`
starts the shipped image itself under `PRAXIS_REQUIRE_FIPS=1`, drives those
same listener probes against it from outside (including a real ExtProc gRPC
exchange over the approved TLS), and checks the startup line.

A hand check on the FIPS host remains a two-liner:

```console
cat /proc/sys/crypto/fips_enabled                        # 1
podman run --rm -e PRAXIS_REQUIRE_FIPS=1 \
    -v $(pwd)/examples/praxis-extproc.yaml:/etc/praxis/extproc.yaml:ro \
    quay.io/opendatahub/odh-praxis-extproc:odh-stable --validate -c /etc/praxis/extproc.yaml
```

The second command exits 0 only when the provider and the kernel both report
FIPS mode. The startup line above is logged when the real workload starts,
so run it the same way with `PRAXIS_REQUIRE_FIPS=1` and keep that line as
evidence; the `fips` health service says the same thing to a probe.

On a developer machine the same behavior tests run their non-approved
branches, and the approved branches can be exercised without a FIPS host by
activating the FIPS provider per process:

```console
CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER="env OPENSSL_CONF=$PWD/xtask/assets/fips/fips-provider.cnf" \
    cargo test --test fips
```

That simulates the provider, not the host: only the FIPS host run proves the
kernel flag, the positive `PRAXIS_REQUIRE_FIPS` path, RHEL's boot-time
module integrity self-tests, and behaviour under the host-wide `FIPS`
crypto policy.

## The runner job

The `fips-host` job of the `FIPS` workflow runs on a self-hosted RHEL 9
runner in FIPS mode, selected by the labels `fips` and `rhel`. It tests the
exact image the hosted `ubi-image` job built and scanned, handed over as an
artifact and checked by image id, then runs `make fips-host-check`,
`make test-fips-host` and `make fips-runtime-probe` through
`.github/actions/fips-host`, keeping the attestation and the probe log as
artifacts.

The runner needs `git`, `make`, `podman` (rootless), `gnupg2`, `gcc`,
`gcc-c++`, `cmake` and `openssl-devel`, checked up front by
`.github/actions/fips-runner-check`, and must be registered with this
repository (it lives in a different GitHub organization than the praxis
runner group, so the same machine needs a runner registration for this
repository too). The job never runs a fork's code (same-repository pull
requests only); the runner takes one job at a time, which serializes it
with the praxis and praxis-ai runs sharing the machine. The first run
builds the `praxis-extproc-fips-host-*` cache volumes cold and is slow;
later runs are incremental.

The module build the UBI 9 images currently carry is in validation with
NIST rather than on an active certificate; `fips-host-check` grades it
against `xtask/assets/fips/certified-modules.json` and says so as a
warning. `FIPS_HOST_CHECK_ARGS=--require-certified` turns that into a
failure for deployments that must not run ahead of the certificate.

## Scope and exemptions

Every crypto-adjacent component in the image, and why it is compliant:

| Component | Use | Disposition |
|---|---|---|
| openssl, openssl-sys, tokio-openssl | the ExtProc listener's TLS; dynamic link to the system `libcrypto.so.3` | compliant |
| rustls, rustls-webpki, rustls-pki-types, rustls-pemfile, tokio-rustls, rustls-native-certs, openssl-probe | TLS protocol engine, X.509 path building, PEM parsing and locating the OS trust store for the filters' subrequests; no cryptography of their own | compliant through the OpenSSL provider |
| rustls-openssl (published from the Pingora fork as `quixotic-plecostomus-rustls-openssl`) | the provider, installed by `src/fips.rs` | compliant |
| tonic, hyper | gRPC and HTTP; built without their TLS features | no cryptography |
| rand, rand_chacha, chacha20 | request ids, load-balancer picks (rand's ChaCha-based RNG) | not security functions |
| ahash, crc32fast | hash maps, gzip checksums | not security functions |
| x509-parser (parsing only, no `verify` feature) | peer certificate fields in the Pingora fork | parse only |
| subtle, zeroize | constant-time comparison, wiping | helpers |
| the `aws_sigv4_sign` filter (feature `aws-sigv4`) | AWS Signature V4 request signing | compliant: praxis-ai signs through the system OpenSSL (`openssl::hash`, `openssl::sign`); the `aws-sigv4` crate is only its dev-time test oracle |
| sqlx-core (the Responses store) | migration checksums use sha2 | not in the FIPS build (feature `responses-store`) |
| sqlx-postgres, rmcp, tiktoken-rs (everything on the store) | SCRAM authentication with md-5, hmac, sha2 and hkdf; reqwest with aws-lc-rs behind the MCP client; the tokenizer | not in the FIPS build (feature `responses-full`) |
| the praxis policy engine (praxis-policy) | its runtime and JWT, OAuth, Valkey builtins carry aws-lc, sha2 and hmac | not in the FIPS build (feature `policy-engine`) |
| rcgen, reqwest with native-tls | test fixtures and clients | development only, absent from the shipped binary and its manifest |

The report and Red Hat's scanner both confirm the last row on every build:
the embedded crate manifest lists none of the denied crates, and the binary
defines no symbol of a bundled crypto backend.

## Status of the dependency pins

The crypto picture above began as the praxis FIPS work
([praxis-proxy/praxis#1254]), which praxis releases have carried since
0.7.0. The praxis crates now come from crates.io at the same 0.7 spec
praxis-ai pins, so the `PipelineExtension` types unify, and praxis-ai is
pinned at a `main` commit that builds against that release; the temporary
`[patch.crates-io]` this section used to describe is gone, and `deny.toml`
no longer allows a praxis git source. Any praxis before 0.7 brings back the
0.9 Pingora fork, whose rustls crate carries a ring provider, and
`make fips-deps` fails on it.

[praxis-proxy/praxis#1254]: https://github.com/praxis-proxy/praxis/pull/1254
