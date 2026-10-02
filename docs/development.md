# Development

Guide for building, testing, and contributing to
Praxis ExtProc.

## Prerequisites

- Rust stable 1.96+ (edition 2024)
- Rust nightly (for `rustfmt`; `group_imports` and
  `imports_granularity` are nightly-only)
- Docker or Podman (container builds; the FIPS checks,
  the base image signature verification and Red Hat's
  scanner need Podman on Linux)
- Go 1.26+ and the OpenShift CLI `oc` (`make fips-scanner`,
  `make fips-scan`; optional)
- [KIND] (local Kubernetes testing)

[KIND]: https://kind.sigs.k8s.io/

## Build Commands

```console
make build       # cargo build
make release     # cargo build --release
make test        # cargo test
make lint        # clippy + nightly fmt check
make fmt         # cargo +nightly fmt
make doc         # rustdoc with -D warnings
make audit       # cargo audit + cargo deny check
make container          # debug container image
make container-release  # release container image (the FIPS build)
make fips-deps          # dependency graph vs Red Hat's crypto denylist
make fips-check         # build on UBI 9 with Red Hat's toolchain, print the report
make fips-scan          # Red Hat's scanner against the image, warnings fatal
```

See [FIPS Tooling](#fips-tooling) for the whole set.

Run a single test:

```console
cargo test -- test_name
```

## Project Structure

```text
src/
  lib.rs               # Crate root, module declarations
  bin/
    praxis_extproc.rs  # Binary entry point, CLI, startup
  adapter.rs           # ExtProc ↔ Praxis type translation
  config.rs            # YAML config loading, pipeline build
  error.rs             # Error types (thiserror)
  health.rs            # gRPC health check service
  metrics.rs           # Prometheus metrics endpoint
  response.rs          # ProcessingResponse builders + chunking
  server.rs            # ExternalProcessor gRPC implementation
  tls.rs               # TLS configuration
  fips.rs              # The crypto provider, FIPS signals, PRAXIS_REQUIRE_FIPS
xtask/
  src/fips/            # cargo xtask fips: report, verify-image, signature-store
  assets/fips/         # Red Hat's release key, its signature store entry
tests/
  grpc_server.rs       # In-process gRPC server tests
  services.rs          # Health and metrics service tests
  integration.rs       # Kubernetes integration tests
examples/
  praxis-extproc.yaml  # Example ExtProc configuration
  envoy.yaml           # Example Envoy configuration
  branch-chains.yaml   # Branch chain example
deploy/
  base/                # Shared Kustomize base
    instance/          # Deployment + Service (hardened, non-root)
    config/            # ConfigMap with BBR + IPP filter chains
  overlays/
    demo/              # Local KIND development overlay
      workload/        # Namespace, TLS-disabled config patch
      test/            # Echo backend, Gateway, HTTPRoute, EnvoyFilter
    odh/               # OpenDataHub production overlay
      rbac/            # ServiceAccount, ClusterRole, ClusterRoleBinding
      networking/      # NetworkPolicy (ingress/egress restrictions)
      pre-processing/  # Second Deployment for pre-auth model extraction
      after/           # Post-auth Deployment + DestinationRule (TLS)
hack/
  kind-config.yaml     # KIND cluster configuration
  setup-kind.sh        # KIND cluster setup script
  smoke-test.sh        # End-to-end smoke test
  teardown-kind.sh     # KIND cluster teardown
```

## Local Development with KIND

Set up a full local environment with Istio and the
ExtProc server deployed:

```console
make dev-env
```

This creates a KIND cluster, installs Istio, deploys
the ExtProc server, and configures an EnvoyFilter to
wire Envoy's ext_proc filter to the server.

### Iterative Development

After the initial setup, rebuild and redeploy:

```console
make dev-push
```

This rebuilds the container image, loads it into
KIND, and restarts the deployment.

Run integration tests against the running cluster:

```console
make dev-integration
```

### Smoke Test

Run end-to-end verification:

```console
make smoke-test
```

Tear down:

```console
make kind-down
```

### Environment Variables

| Variable | Default |
| --- | --- |
| `KIND_CLUSTER_NAME` | `praxis-extproc` |
| `EXTPROC_IMAGE` | `praxis-extproc:dev` |

## Testing

### Unit Tests

```console
make test
```

Unit tests are embedded in each source module. They
cover config parsing, adapter translation, response
building, body chunking, and TLS configuration.

### gRPC Server Tests

`tests/grpc_server.rs` starts the ExtProc server
in-process and exercises the full gRPC stream
lifecycle: request headers, request body, response
headers, response body, trailers, and rejection
scenarios.

### Service Tests

`tests/services.rs` tests the health and metrics
auxiliary services independently.

### Integration Tests

Require a running Kubernetes cluster with Istio and
the ExtProc server deployed:

```console
make test-integration
```

Gated behind the `integration` feature flag. These
tests verify end-to-end behavior through Envoy's
ext_proc filter in a real cluster.

Run with verbose output:

```console
make test-integration V=1
```

## Container Build

```console
make container          # debug binary (in-container)
make container-release  # release binary (in-container, embedded crate manifest)
```

The image is the FIPS build; see [FIPS](fips.md) for
what that means and what it leaves out. Multi-stage
`Containerfile`: `ubi9/ubi` builder (AppStream
`rust-toolset` 1.92, `OPENSSL_NO_VENDOR=1` so the
binary links UBI9 `libcrypto.so.3`) and
`ubi9/ubi-minimal` at runtime, which already ships
`openssl-libs` and the FIPS provider module; nothing
is installed into it. Both bases are pinned by digest
and, with podman, their Red Hat signatures are
verified before every build. The release build embeds
the list of compiled crates with cargo-auditable, from
cargo's SBOM precursor, which Red Hat's scanner reads.
Cargo is invoked with `--ignore-rust-version` because
Red Hat's toolset trails the declared rust-version.
Builds for the host architecture. The runtime image
runs as UID 1001 (OpenShift-friendly numeric non-root
user). Konflux (`.tekton/`) builds the same file with
its defaults, which are the FIPS feature set.

## CI

GitHub Actions workflow (`.github/workflows/tests.yaml`)
runs on every push and pull request:

- Format check (`cargo +nightly fmt --check`)
- Clippy (`cargo clippy -- -D warnings`)
- Tests (`cargo test`)
- Doc build (`RUSTDOCFLAGS="-D warnings" cargo doc`)
- Audit (`cargo audit`, `cargo deny check`)

The FIPS workflow (`.github/workflows/fips.yaml`) runs
the checks below on every push and pull request: the
dependency graph against Red Hat's denylist, the
compliance report from a build on UBI 9, the product
image, its smoke run, and Red Hat's scanner with
warnings fatal.

## FIPS Tooling

Local, reproducible checks that the image is on the
path to FIPS 140-3 compliance on Red Hat Enterprise
Linux; what it means for a deployment is in
[FIPS](fips.md). The checks mirror what Red Hat's
release scanner (`openshift/check-payload`, Rust support
in its PR #360) looks at, so a clean local report is a
strong predictor of a clean scan. Everything here is a
`cargo xtask fips` command (`xtask/src/fips/`), wrapped
by a Makefile target; the same report runs inside the
`Containerfile`'s report stage.

| Command | Makefile | Purpose |
|---|---|---|
| `cargo xtask fips report [--deps-only] [--features LIST] [--offline] [--out FILE] [BINARY]` | `fips-deps`, `fips-report`, `fips-check` | The compliance report: environment, dependency graph, binary structure, source guards, with a reason and a pointer for every finding. Exit status 1 while findings remain. |
| `cargo xtask fips verify-image REFERENCE` | `fips-verify-image` (run by `container-release` under podman and by `fips-check` first) | Refuses any base image that is not digest-pinned, from `registry.access.redhat.com`, and signed by Red Hat's release key. |
| `cargo xtask fips signature-store [--install]` | `fips-signature-store` | Whether podman's `registries.d` names Red Hat's signature store, without which every Red Hat image looks unsigned; `--install` adds the bundled entry for the current user on hosts whose podman packaging ships none (Debian, Ubuntu, GitHub's runners). CI runs it before `fips-verify-image`. |
| `check-payload scan image ...` | `fips-scanner`, `fips-scan` | Red Hat's own scanner at a pinned revision, run against the image with warnings fatal: the actual gate. |

The Makefile targets, in the order CI runs them:

```console
make fips-deps           # dependency graph vs the denylist (seconds, no build)
make fips-scanner        # build check-payload at its pinned revision (needs Go)
make fips-signature-store  # once on Debian/Ubuntu hosts
make fips-verify-image   # the pinned UBI 9 bases are Red Hat's (digest + signature)
make fips-check          # build on UBI 9 with Red Hat's toolchain, print the report
make container-release   # the product image
make fips-smoke          # run it once: the example config validates
make fips-scan           # Red Hat's scanner, warnings fatal (needs oc on PATH)
```

For the edit-compile loop there are `build-fips`,
`release-fips`, `check-fips`, `lint-fips` and
`test-fips`, which build the FIPS feature set into
`target/fips` so it is never confused with the default
build, and `fips-report` runs the report against
`target/fips/release/praxis-extproc`.

### What the report checks

1. **Dependency graph**: no crate on the scanner's
   `rust_denied_crypto` list (`ring`, `aws-lc-rs`,
   `sha2`, `hmac`, ...) in the binary's normal
   dependency graph, resolved for the FIPS feature set.
   `--deps-only` stops here.
2. **Binary**: links the system `libcrypto.so.3`
   dynamically, defines no symbol of a bundled crypto
   backend (`ring_core_`, `aws_lc_`, `BORINGSSL_`,
   `OPENSSL_`), imports its cryptography from the system
   OpenSSL and imports only reviewed OpenSSL symbols —
   every `name@OPENSSL_version` symbol it imports is on
   the reviewed allowlist in
   `xtask/assets/fips/openssl-nondeprecated-symbols.txt`
   (every non-deprecated libcrypto/libssl export through
   OpenSSL 3.5), so an unlisted or deprecated versioned
   OpenSSL import, or a binary with no GNU symbol-version
   table, is a finding — carries the cargo-auditable
   manifest (`.dep-v0`, built from cargo's SBOM
   precursor, listing no denied crate) and the rustc
   producer string. A binary that is missing, unreadable
   or not an ELF file is a finding, not a skipped check.
3. **Source guards**: the application never enables a
   FIPS provider itself, never uses OpenSSL's legacy
   (non-provider) digest API, never vendors or
   statically links OpenSSL.

### Data compiled into xtask

From `xtask/assets/fips/`:

| File | Purpose |
|---|---|
| `redhat-release-key-2.asc` | Red Hat, Inc. (release key 2), the GPG key Red Hat signs its container images with. Downloaded on 2026-09-22 from `https://access.redhat.com/security/data/fd431d51.txt`; its fingerprint, `567E 347A D004 4ADE 55BA 8A5F 199E 2F91 FD43 1D51`, matches the one Red Hat publishes at `https://access.redhat.com/security/team/key`. `verify-image` recomputes the fingerprint on every run (RFC 4880 v4, in Rust) and refuses to proceed if it differs; `cargo test -p xtask` checks the same. podman's own signature check needs gnupg installed. |
| `registry.access.redhat.com.yaml` | The `registries.d` entry that tells podman where Red Hat's signature store is. podman reads only its own `registries.d` (`~/.config/containers/registries.d` when it exists, else `/etc/containers/registries.d`); Fedora and RHEL ship the same entry in containers-common, Debian and Ubuntu ship no `registries.d` at all, which `make fips-signature-store` fixes for the current user. |
| `fips-provider.cnf` | An `OPENSSL_CONF` that activates the RHEL FIPS provider for one process, used by the report to probe FIPS behaviour on hosts that are not in FIPS mode. Test infrastructure only; the application never enables FIPS itself. |

### cargo-auditable and cargo's SBOM precursor

Red Hat's scanner finds pure-Rust cryptography through
the crate list that `cargo auditable build` embeds in
the binary (the `.dep-v0` section); a binary without
it is graded inconclusive. The UBI builder installs
`cargo-auditable` from crates.io at the version pinned
in the `Containerfile` (`CARGO_AUDITABLE_VERSION`), and
`make release-fips` uses it when it is installed
locally (`cargo install cargo-auditable --version 0.7.6
--locked`). It is maintained by the Rust Secure Code
Working Group and embeds data only, never code.

The manifest has to list exactly the crates compiled
in: the scanner fails on a denied crate's name alone.
On a stable toolchain cargo-auditable derives the list
from `cargo metadata`, which unifies features across
every workspace member and activates weak features
(`dep?/feature`) the real build never turns on; with
rustls that puts `ring` in the manifest of a binary
that never compiled it. Cargo's SBOM precursor
(`build.sbom`, unstable behind `-Zsbom`) is written by
cargo's own unit graph and is exact, so the release
build runs

```console
RUSTC_BOOTSTRAP=1 CARGO_BUILD_SBOM=true cargo auditable -Zsbom     --config 'env.RUSTC_BOOTSTRAP.value="-1"'     --config 'env.RUSTC_BOOTSTRAP.force=true'     build --release -p praxis-extproc ...
```

`RUSTC_BOOTSTRAP=1` lets a stable cargo accept the
`-Z` flag; the two `env` overrides hand rustc and every
build script `RUSTC_BOOTSTRAP=-1`, which forbids
unstable features, so the code compiled is the stable
code. The report checks the manifest's `format` field
(8 when it came from the precursor), so a build that
silently fell back to `cargo metadata` is a finding.
Cargo before 1.99 does not relink a binary when only
the SBOM setting changed (rust-lang/cargo#15695), so
both recipes remove the old binary first. Drop the
whole workaround once `build.sbom` is stable
(rust-lang/cargo#13709).

### Red Hat's scanner

`make fips-scanner` fetches the pinned commit of
`openshift/check-payload` (`CHECK_PAYLOAD_REV` in the
Makefile, the head of its PR #360, fetched by commit so
a rewrite of the PR cannot break the build) and builds
it the way upstream does (`CGO_ENABLED=0 go build`,
vendored modules) into `target/fips/check-payload/`.
`make fips-scan` runs it against the image from podman's
store (under `podman unshare` when podman is rootless)
with `--fail-on-warnings`, so an inconclusive verdict
such as a missing manifest fails, as it does in Red
Hat's gated scans. Both need a Linux podman, rootless
or root, not a podman machine, and the scan needs the
OpenShift CLI (`oc`) on `PATH`: the scanner refuses to
start without it, even though an image scan never runs
it. CI installs a fixed `oc` release from Red Hat's
mirror, checksum-verified. Point `CHECK_PAYLOAD` at
another build to use it instead.

### Updating the pinned base images

The digests live in the `Makefile` (`FIPS_UBI9_DIGEST`,
`FIPS_UBI9_MINIMAL_DIGEST`) and, as defaults, in the
`Containerfile`. To move to a newer UBI 9:

```console
curl -sI -H 'Accept: application/vnd.docker.distribution.manifest.list.v2+json'   https://registry.access.redhat.com/v2/ubi9/ubi/manifests/latest | grep -i docker-content-digest
cargo xtask fips verify-image registry.access.redhat.com/ubi9/ubi@sha256:<new digest>
```

Only update both places once the verification passes.

## Project Management

This repository uses a consistent workflow for
planning, prioritizing, and tracking work.

### Milestones

Milestones represent a body of work toward a shared
goal (e.g. a release, a feature area, or a hardening
pass). Every issue and pull request should belong to
a milestone. Milestones provide scope boundaries and
help answer "what ships together?"

### Priority Labels

Priority labels indicate the order in which work
within a milestone should be addressed. Every issue
should have exactly one priority label:

| Label | Description |
| --- | --- |
| `priority/critical` | Must be worked on immediately before anything else |
| `priority/high` | Needs to be worked on immediately, defer to criticals |
| `priority/medium` | Resolve after high and critical |
| `priority/low` | Resolve after all other priority levels |

When picking up work, address issues in priority
order: critical first, then high, medium, and low.

### Project Boards

GitHub project boards visualize the state of work
across milestones. Use boards to track issues through
their lifecycle (backlog, in progress, in review,
done). Boards are the primary tool for stand-ups and
status checks.

## Coding Conventions

See [conventions.md](conventions.md) for the full
coding standards, including file ordering, test
conventions, separator comment format, and
documentation requirements.
