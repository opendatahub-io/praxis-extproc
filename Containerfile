# Multi-stage build for praxis-extproc.
#
# Build:
#   docker build -t praxis-extproc:dev -f Containerfile .
#
# Run:
#   docker run -p 50051:50051 -p 50052:50052 -p 9090:9090 \
#     -v $(pwd)/examples/praxis-extproc.yaml:/etc/praxis/extproc.yaml \
#     praxis-extproc:dev -c /etc/praxis/extproc.yaml

# ---------------------------------------------------------------------------
# Builder
# ---------------------------------------------------------------------------

FROM rust:1.94-alpine AS builder

RUN apk add --no-cache musl-dev cmake make perl

WORKDIR /build
COPY . .

RUN cargo build --release --bin praxis-extproc \
    && strip target/release/praxis-extproc

# ---------------------------------------------------------------------------
# Runtime
# ---------------------------------------------------------------------------

FROM alpine:3.22 AS runtime

RUN addgroup -S praxis && adduser -S praxis -G praxis

COPY --from=builder /build/target/release/praxis-extproc /usr/local/bin/praxis-extproc

USER praxis

EXPOSE 50051 50052 9090

ENTRYPOINT ["praxis-extproc"]
CMD ["-c", "/etc/praxis/extproc.yaml"]
