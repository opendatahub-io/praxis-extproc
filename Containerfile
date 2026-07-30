# syntax=docker/dockerfile:1
# Multi-stage build for praxis-extproc.
#
# Builder: Debian Bookworm on $BUILDPLATFORM, cross-compiles for
# $TARGETPLATFORM (linux/amd64, linux/arm64).
# Runtime: ubi10/ubi-minimal for the target platform.
#
# Build (host platform):
#   make container-release
#
# Multi-arch:
#   make container-release PLATFORMS=linux/amd64,linux/arm64
#
# Run:
#   docker run -p 50051:50051 -p 50052:50052 -p 9090:9090 \
#     -v $(pwd)/examples/praxis-extproc.yaml:/etc/praxis/extproc.yaml \
#     praxis-extproc:dev -c /etc/praxis/extproc.yaml

# ---------------------------------------------------------------------------
# Builder
# ---------------------------------------------------------------------------

FROM --platform=$BUILDPLATFORM docker.io/library/debian:bookworm-slim AS builder

ARG BUILDPLATFORM
ARG TARGETPLATFORM
ARG CARGO_PROFILE=release

RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates curl \
      build-essential cmake make perl pkg-config \
      gcc-x86-64-linux-gnu g++-x86-64-linux-gnu \
      gcc-aarch64-linux-gnu g++-aarch64-linux-gnu \
      libc6-dev-amd64-cross \
      libc6-dev-arm64-cross \
    && rm -rf /var/lib/apt/lists/*

RUN case "${TARGETPLATFORM}" in \
      linux/amd64) \
        echo "x86_64-unknown-linux-gnu" > /tmp/rust_target; \
        echo "x86_64-linux-gnu-gcc" > /tmp/linker ;; \
      linux/arm64) \
        echo "aarch64-unknown-linux-gnu" > /tmp/rust_target; \
        echo "aarch64-linux-gnu-gcc" > /tmp/linker ;; \
      *) \
        echo "unsupported TARGETPLATFORM=${TARGETPLATFORM}" >&2; \
        exit 1 ;; \
    esac

ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --default-toolchain none --profile minimal

WORKDIR /build
COPY rust-toolchain.toml .
RUN rustup show \
    && rustup target add "$(cat /tmp/rust_target)"

COPY . .

RUN set -eu; \
    TARGET=$(cat /tmp/rust_target); \
    LINKER=$(cat /tmp/linker); \
    export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=x86_64-linux-gnu-gcc; \
    export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc; \
    case "${TARGET}" in \
      x86_64-unknown-linux-gnu) \
        STRIP=x86_64-linux-gnu-strip; \
        export CC_x86_64_unknown_linux_gnu=${LINKER} \
               CXX_x86_64_unknown_linux_gnu=x86_64-linux-gnu-g++ ;; \
      aarch64-unknown-linux-gnu) \
        STRIP=aarch64-linux-gnu-strip; \
        export CC_aarch64_unknown_linux_gnu=${LINKER} \
               CXX_aarch64_unknown_linux_gnu=aarch64-linux-gnu-g++ ;; \
    esac; \
    if [ "${CARGO_PROFILE}" = "release" ]; then \
      cargo build --release --bin praxis-extproc --target "${TARGET}"; \
      BIN="target/${TARGET}/release/praxis-extproc"; \
      "${STRIP}" "${BIN}"; \
    elif [ "${CARGO_PROFILE}" = "debug" ]; then \
      cargo build --bin praxis-extproc --target "${TARGET}"; \
      BIN="target/${TARGET}/debug/praxis-extproc"; \
    else \
      echo "unsupported CARGO_PROFILE=${CARGO_PROFILE}" >&2; \
      exit 1; \
    fi; \
    cp "${BIN}" /praxis-extproc

# ---------------------------------------------------------------------------
# Runtime
# ---------------------------------------------------------------------------

FROM registry.access.redhat.com/ubi10/ubi-minimal

RUN microdnf install -y shadow-utils \
    && microdnf clean all \
    && groupadd -r praxis \
    && useradd -r -g praxis praxis

COPY --from=builder /praxis-extproc /usr/local/bin/praxis-extproc

USER praxis

EXPOSE 50051 50052 9090

ENTRYPOINT ["praxis-extproc"]
CMD ["-c", "/etc/praxis/extproc.yaml"]
