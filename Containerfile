# syntax=docker/dockerfile:1

# ------------------------------------------------------------------------------
# praxis-extproc on Red Hat Universal Base Image 9: the FIPS build
# ------------------------------------------------------------------------------
#
# This is the product image, and it is the FIPS build: the defaults below
# produce it with no Makefile involvement, which is how Konflux (.tekton/)
# builds it. Three stages:
#
#   builder   builds the FIPS feature set with Red Hat's own Rust toolchain
#             and OpenSSL (make container-release, make fips-check)
#   report    runs `cargo xtask fips report` on that binary and keeps the
#             result (make fips-check)
#   runtime   the shippable image on ubi9/ubi-minimal (make container-release)
#
# The FIPS build differs from a default `cargo build` only in its cargo
# features: it leaves out what carries pure-Rust cryptography (the praxis
# policy engine, the Responses store; the SigV4 signer signs through the
# system OpenSSL and stays in). CARGO_FEATURES defaults to the Makefile's
# FIPS_FEATURES; keep the two in sync.
#
# Both base images are pinned by digest and their Red Hat signatures are
# verified by `make fips-verify-image` before any podman build here. Update
# the digests here and in the Makefile together, after
# `cargo xtask fips verify-image` has accepted the new ones.
#
# Run:
#   podman run -p 50051:50051 -p 50052:50052 -p 9090:9090 \
#     -v $(pwd)/examples/praxis-extproc.yaml:/etc/praxis/extproc.yaml \
#     praxis-extproc:dev -c /etc/praxis/extproc.yaml

ARG UBI9_DIGEST=sha256:a4b9ec09b1e790a53ef25b7777c539976abe519248264298e5194dcbceac8c31
ARG UBI9_MINIMAL_DIGEST=sha256:8ebe2ad8fdf3cab3e5a53c1edc69194c98209cfadab24b884f4ad9ebcf7bbbfc

# Mirrors FIPS_FEATURES in the Makefile.
ARG CARGO_FEATURES="responses,aws-sigv4"
# release (the product image, with the embedded crate manifest) or debug (the
# edit-compile loop; make container).
ARG CARGO_PROFILE=release
# Red Hat's rust-toolset (1.92) trails the declared rust-version; the build
# passes --ignore-rust-version so the toolchain Red Hat ships is what gets
# exercised. If the tree needs a newer compiler, this step is where it shows.
ARG CARGO_ARGS="--ignore-rust-version"

# ------------------------------------------------------------------------------
# Stage 1: build with Red Hat's toolchain
# ------------------------------------------------------------------------------

FROM registry.access.redhat.com/ubi9/ubi@${UBI9_DIGEST} AS builder

# Everything comes from Red Hat's UBI repositories (GPG-checked by dnf):
#   rust-toolset   Red Hat's rustc and cargo
#   openssl-devel  headers so openssl-sys links the system libcrypto
RUN dnf install -y --setopt=install_weak_deps=False \
        rust-toolset openssl-devel gcc gcc-c++ cmake make \
    && dnf clean all

# Never vendor OpenSSL; the system library is the validated module.
ENV OPENSSL_NO_VENDOR=1 \
    CARGO_HOME=/cargo

# cargo-auditable embeds the list of crates compiled into the binary, which is
# the manifest Red Hat's scanner reads to find pure-Rust cryptography that
# leaves no symbol. Not a Red Hat package: installed from crates.io at a pinned
# version (Rust Secure Code Working Group; see docs/fips.md).
ARG CARGO_AUDITABLE_VERSION=0.7.6
RUN --mount=type=cache,id=praxis-extproc-registry,target=/cargo/registry \
    cargo install cargo-auditable --version "${CARGO_AUDITABLE_VERSION}" --locked

WORKDIR /src

# The whole workspace, so the build resolves the committed lockfile as is
# (--locked) and the report stage can run `cargo xtask fips`.
COPY Cargo.toml Cargo.lock ./
COPY proto proto
COPY src src
COPY xtask xtask

ARG CARGO_FEATURES
ARG CARGO_PROFILE
ARG CARGO_ARGS

# The embedded manifest must list exactly the crates compiled in. On a stable
# toolchain cargo-auditable derives it from `cargo metadata`, which activates
# weak features the build never enables (rustls-webpki's `ring?/alloc` puts
# ring in the manifest of a binary that never compiled it) and unifies
# features across workspace members; the scanner fails on the name alone.
# Cargo's SBOM precursor is the exact list but unstable (-Zsbom):
# RUSTC_BOOTSTRAP=1 lets Red Hat's stable cargo accept the flag, and the env
# overrides hand rustc and every build script RUSTC_BOOTSTRAP=-1, which forbids
# unstable features, so the code compiled is the stable code. Drop this once
# cargo's `build.sbom` is stable (rust-lang/cargo#13709).
#
# cargo before 1.99 does not relink a binary when only the SBOM setting
# changed (rust-lang/cargo#15695, fixed by #17216), so the old binary in the
# cached target goes first. Drop the clean once rust-toolset is 1.99 or newer.
#
# The release profile strips the binary itself (Cargo.toml); the debug build
# carries no manifest, it is the edit-compile loop.
RUN --mount=type=cache,id=praxis-extproc-registry,target=/cargo/registry \
    --mount=type=cache,id=praxis-extproc-target,target=/src/target \
    set -eu; \
    case "${CARGO_PROFILE}" in \
      release) \
        cargo clean --release -p praxis-extproc; \
        RUSTC_BOOTSTRAP=1 CARGO_BUILD_SBOM=true \
          cargo auditable -Zsbom \
            --config 'env.RUSTC_BOOTSTRAP.value="-1"' \
            --config 'env.RUSTC_BOOTSTRAP.force=true' \
            build --release --locked -p praxis-extproc \
            --no-default-features --features "${CARGO_FEATURES}" ${CARGO_ARGS}; \
        BIN=target/release/praxis-extproc ;; \
      debug) \
        cargo build --locked -p praxis-extproc \
            --no-default-features --features "${CARGO_FEATURES}" ${CARGO_ARGS}; \
        BIN=target/debug/praxis-extproc ;; \
      *) echo "unsupported CARGO_PROFILE=${CARGO_PROFILE}" >&2; exit 1 ;; \
    esac; \
    mkdir -p /out; \
    cp "${BIN}" /out/praxis-extproc

# ------------------------------------------------------------------------------
# Stage 2: compliance report
# ------------------------------------------------------------------------------

# Only built when this stage is the target (make fips-check), so the product
# image never compiles xtask. The report runs at build time, while the cargo
# cache is mounted, so `cargo tree` can resolve the graph offline. The exit
# status is kept so the image can be run to print the report and fail
# accordingly.
FROM builder AS report

ARG CARGO_FEATURES
ARG CARGO_ARGS

RUN --mount=type=cache,id=praxis-extproc-registry,target=/cargo/registry \
    --mount=type=cache,id=praxis-extproc-target,target=/src/target \
    { mkdir -p /fips; \
      cargo run --quiet --locked -p xtask ${CARGO_ARGS} -- \
        fips report /out/praxis-extproc --features "${CARGO_FEATURES}" --offline --out /fips/report.txt; \
      echo $? > /fips/status; } \
    ; true

CMD ["sh", "-c", "cat /fips/report.txt; exit $(cat /fips/status)"]

# ------------------------------------------------------------------------------
# Stage 3: runtime
# ------------------------------------------------------------------------------

FROM registry.access.redhat.com/ubi9/ubi-minimal@${UBI9_MINIMAL_DIGEST} AS runtime

LABEL org.opencontainers.image.source="https://github.com/opendatahub-io/praxis-extproc" \
    org.opencontainers.image.description="Envoy ExtProc server for Praxis filter pipelines (FIPS build on UBI 9)" \
    org.opencontainers.image.licenses="Apache-2.0"

# ubi9/ubi-minimal already ships what the binary needs at runtime, all from
# Red Hat: openssl-libs (libcrypto.so.3, libssl.so.3, the same SONAME the
# builder linked), the FIPS provider module (openssl-fips-provider-so) and
# ca-certificates. Nothing is installed here, so the image stays exactly what
# Red Hat signed plus the binary.

COPY --from=builder --chown=root:root --chmod=0555 \
    /out/praxis-extproc /usr/local/bin/praxis-extproc

# OpenShift-friendly numeric non-root user.
USER 1001

EXPOSE 50051 50052 9090

ENTRYPOINT ["praxis-extproc"]
CMD ["-c", "/etc/praxis/extproc.yaml"]
