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

Deployment manifests use [Kustomize] with a shared
base and environment-specific overlays.

Deploy the workload to a local cluster:

```console
kubectl apply -k deploy/overlays/demo/workload
```

This creates:

- A `praxis-extproc` namespace
- A ConfigMap with BBR (pre-auth) and IPP (post-auth)
  filter chain configurations
- A `payload-processing` Deployment running the
  ExtProc server (hardened: non-root, read-only
  filesystem, resource limits)
- A ClusterIP Service on port 9004 (gRPC)

Deploy test resources (echo backend + Istio gateway):

```console
kubectl apply -k deploy/overlays/demo/test
```

Or deploy everything in one step:

```console
kubectl apply -k deploy/overlays/demo
```

This additionally creates an echo backend, an Istio
Gateway, an HTTPRoute, and an [EnvoyFilter] that
wires Envoy's ext_proc HTTP filter to the ExtProc
server with `BUFFERED` mode for request and response
bodies.

Preview rendered manifests without applying:

```console
make manifests-demo
make manifests-odh
```

Verify the deployment:

```console
kubectl -n praxis-extproc rollout status \
    deployment/payload-processing
```

[Kustomize]: https://kustomize.io/
[EnvoyFilter]: https://istio.io/latest/docs/reference/config/networking/envoy-filter/

### Production Deployment (OpenDataHub)

For production MaaS environments, use the `odh`
overlay which adds:

- Dual ExtProc instances (pre-auth BBR +
  post-auth IPP)
- RBAC with least-privilege read-only access to
  CRDs and secrets
- NetworkPolicy restricting ingress to gateway
  pods and monitoring
- DestinationRules with SIMPLE TLS and explicit SNI
- EnvoyFilter anchored around Kuadrant auth
  (supports Istio <=1.25 through >=1.30 and RHCL)

```console
kubectl apply -k deploy/overlays/odh
```

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
make container-release
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
