# Getting Started

This guide covers running the Praxis ExtProc server
locally alongside Envoy, and deploying to Kubernetes.

## Prerequisites

- Rust stable 1.94+
- [Envoy] proxy (for local testing)

[Envoy]: https://www.envoyproxy.io/docs/envoy/latest/start/install

## Local Quickstart

Build the server:

```console
make build
```

Start with the example config:

```console
./target/debug/praxis-extproc \
    -c examples/praxis-extproc.yaml
```

The server listens on three ports:

| Port | Service |
| --- | --- |
| 50051 | gRPC ExtProc |
| 50052 | gRPC health check |
| 9090 | Prometheus metrics |

### Wire Envoy

Start Envoy with the example config that connects to
the ExtProc server:

```console
envoy -c examples/envoy.yaml
```

The example Envoy config listens on port 8080 and
forwards requests to a backend on port 3000, with
all headers and bodies sent through the ExtProc
filter.

Test with a running backend:

```console
curl -v http://127.0.0.1:8080/
```

The response should include headers added by the
Praxis filters (e.g. `X-Processed-By: praxis-extproc`,
`X-Request-Id`).

### Validate Configuration

Check a config file without starting the server:

```console
./target/debug/praxis-extproc -t \
    -c examples/praxis-extproc.yaml
```

## Kubernetes Deployment

### Environment Prerequisites

- Kubernetes 1.32+
- kubectl configured for your cluster

### Apply Manifests

```console
kubectl apply -f deploy/namespace.yaml
kubectl apply -f deploy/configmap.yaml
kubectl apply -f deploy/deployment.yaml
kubectl apply -f deploy/service.yaml
```

This creates:

- A `praxis-extproc` namespace
- A ConfigMap with the ExtProc filter configuration
- A Deployment running the ExtProc server
- A Service exposing gRPC (50051), health (50052),
  and metrics (9090)

Verify the deployment:

```console
kubectl -n praxis-extproc rollout status \
    deployment/praxis-extproc
```

### Istio Integration

For Istio service meshes, an [EnvoyFilter] resource
wires Envoy's ext_proc HTTP filter to the ExtProc
server.

Deploy the test resources:

```console
kubectl apply -f deploy/echo.yaml
kubectl apply -f deploy/gateway.yaml
kubectl apply -f deploy/httproute.yaml
kubectl apply -f deploy/envoyfilter.yaml
```

This configures:

- An echo backend in the `praxis-test` namespace
- An Istio Gateway listening on port 8080
- An HTTPRoute routing traffic to the echo backend
- An EnvoyFilter injecting the ext_proc filter before
  the router, pointing to the ExtProc gRPC service

The EnvoyFilter configures `BUFFERED` mode for both
request and response bodies, enabling body-inspecting
filters like `guardrails` and `json_body_field`.

[EnvoyFilter]: https://istio.io/latest/docs/reference/config/networking/envoy-filter/

### Test

```console
GW_IP=$(kubectl -n praxis-test \
    get gateway praxis-test \
    -o jsonpath='{.status.addresses[0].value}')

curl -v http://${GW_IP}:8080/
```

The response should include the `X-Processed-By` and
`X-Praxis` headers injected by the ExtProc filters.

### Container Image

Build the container image:

```console
make container
```

Run directly:

```console
docker run -p 50051:50051 -p 50052:50052 -p 9090:9090 \
    -v $(pwd)/examples/praxis-extproc.yaml:/etc/praxis/extproc.yaml \
    praxis-extproc:dev -c /etc/praxis/extproc.yaml
```

## Local Development with KIND

For a fully automated local environment:

```console
make dev-env
```

See [Development](development.md) for details on
iterative development, smoke tests, and integration
testing.

## Next Steps

- [Architecture](architecture.md): how the ExtProc
  server works internally
- [Configuration](configuration.md): YAML reference
  for filter chains, server, and TLS settings
- [Development](development.md): building, testing,
  and contributing
