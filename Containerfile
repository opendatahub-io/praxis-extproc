# Multi-stage build for praxis-extproc.
#
# Builder: ubi10/ubi with rustup (native cargo on the host platform).
# Runtime: ubi10/ubi-minimal.
#
# Build:
#   make container-release
#
# Run:
#   docker run -p 50051:50051 -p 50052:50052 -p 9090:9090 \
#     -v $(pwd)/examples/praxis-extproc.yaml:/etc/praxis/extproc.yaml \
#     praxis-extproc:dev -c /etc/praxis/extproc.yaml

# ---------------------------------------------------------------------------
# Builder
# ---------------------------------------------------------------------------

FROM registry.access.redhat.com/ubi10/ubi AS builder

ARG CARGO_PROFILE=release

RUN dnf install -y gcc gcc-c++ cmake make perl openssl-devel \
    && dnf clean all

ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --default-toolchain none --profile minimal

WORKDIR /build
COPY rust-toolchain.toml .
RUN rustup show

COPY . .


RUN set -eu; \
    if [ "${CARGO_PROFILE}" = "release" ]; then \
      cargo build --release --bin praxis-extproc; \
      BIN=target/release/praxis-extproc; \
      strip "${BIN}"; \
    elif [ "${CARGO_PROFILE}" = "debug" ]; then \
      cargo build --bin praxis-extproc; \
      BIN=target/debug/praxis-extproc; \
    else \
      echo "unsupported CARGO_PROFILE=${CARGO_PROFILE}" >&2; \
      exit 1; \
    fi; \
    cp "${BIN}" /praxis-extproc

# ---------------------------------------------------------------------------
# Runtime
# ---------------------------------------------------------------------------

FROM registry.access.redhat.com/ubi10/ubi-minimal

RUN microdnf install -y openssl-libs \
    && microdnf clean all

COPY --from=builder /praxis-extproc /usr/local/bin/praxis-extproc

USER 1001

EXPOSE 50051 50052 9090

ENTRYPOINT ["praxis-extproc"]
CMD ["-c", "/etc/praxis/extproc.yaml"]
