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

### OpenShift Deployment

The `demo` overlay above targets a local KIND cluster, so two
things need adjusting for a remote OpenShift cluster:

1. **Image.** The overlay's `praxis-extproc:dev` tag is a local
   image KIND side-loads; a remote cluster cannot pull it. Publish
   the image to the cluster's internal registry instead.
2. **Service Mesh.** The `test` overlay creates an Istio `Gateway`
   (`gatewayClassName: istio`) and an `EnvoyFilter`. These require
   an Istio-based mesh that provides the `istio` GatewayClass and
   the `EnvoyFilter` CRD (e.g. [OpenShift Service Mesh]).

The `openshift` overlay handles the image; the mesh is a one-time
cluster install.

#### 1. Publish the image to the internal registry

Expose the internal registry route (one-time, cluster-admin):

```console
oc patch configs.imageregistry.operator.openshift.io/cluster \
    --type=merge -p '{"spec":{"defaultRoute":true}}'
REGISTRY=$(oc get route default-route \
    -n openshift-image-registry -o jsonpath='{.spec.host}')
```

Build, log in, and push. Pushing creates the `praxis-extproc`
namespace's ImageStream automatically:

```console
make container-release

oc new-project praxis-extproc
podman login -u "$(oc whoami)" -p "$(oc whoami -t)" "$REGISTRY"
podman tag docker.io/library/praxis-extproc:dev \
    "$REGISTRY/praxis-extproc/praxis-extproc:dev"
podman push "$REGISTRY/praxis-extproc/praxis-extproc:dev"
```

If your cluster's ingress uses a self-signed certificate, add its
CA to the local trust store rather than disabling TLS
verification.

#### 2. Install OpenShift Service Mesh 3

Install the operator:

```console
oc apply -f - <<'YAML'
apiVersion: operators.coreos.com/v1alpha1
kind: Subscription
metadata:
  name: servicemeshoperator3
  namespace: openshift-operators
spec:
  channel: stable
  installPlanApproval: Automatic
  name: servicemeshoperator3
  source: redhat-operators
  sourceNamespace: openshift-marketplace
YAML
```

The Subscription installs the operator asynchronously. Wait for its
CSV to reach `Succeeded` before continuing — the `Istio` and
`IstioCNI` CRDs do not exist until the operator has finished
installing:

```console
oc wait --for=condition=InstallSucceeded csv \
    -l operators.coreos.com/servicemeshoperator3.openshift-operators \
    -n openshift-operators --timeout=300s
```

Create the control plane and CNI (CNI is required on OpenShift):

```console
oc apply -f - <<'YAML'
apiVersion: v1
kind: Namespace
metadata: { name: istio-system }
---
apiVersion: v1
kind: Namespace
metadata: { name: istio-cni }
---
apiVersion: sailoperator.io/v1
kind: Istio
metadata: { name: default }
spec:
  namespace: istio-system
---
apiVersion: sailoperator.io/v1
kind: IstioCNI
metadata: { name: default }
spec:
  namespace: istio-cni
YAML

oc wait --for=jsonpath='{.status.state}'=Healthy \
    istio/default istiocni/default --timeout=300s
```

This creates the `istio` GatewayClass and the `EnvoyFilter` CRD.

The `Istio` CR omits `spec.version`, so the operator installs its own
default Istio version. To see which version was selected, check the
resulting revision (`oc get istiorevisions` — the name encodes the
version, e.g. `default-v1-24-3`). To pin a version for
reproducibility, set `spec.version` on the `Istio` CR (e.g.
`version: v1.24.3`).

#### 3. Deploy and verify

The `openshift` overlay is the `demo` overlay with the image
pointed at the in-cluster registry
(`image-registry.openshift-image-registry.svc:5000`, identical on
every OpenShift cluster). The namespace's default service account
already has a pull secret for it, so no imagePullSecret is needed.

```console
oc apply -k deploy/overlays/openshift
oc -n praxis-extproc rollout status \
    deployment/payload-processing
```

The Gateway auto-provisions an Envoy behind a `LoadBalancer`
Service (an ELB on cloud OpenShift). Wait for it, then test:

```console
oc -n praxis-test wait --for=condition=Programmed \
    gateway/praxis-test --timeout=180s

GW=$(oc -n praxis-test get gateway praxis-test \
    -o jsonpath='{.status.addresses[0].value}')
curl -v "http://${GW}:8080/"
```

The load balancer may take a minute to become reachable after the
Gateway reports `Programmed`. The response should be `200 OK` with
the ExtProc-injected `x-praxis` and `x-request-id` headers.

[OpenShift Service Mesh]: https://docs.openshift.com/container-platform/latest/service_mesh/v3x/ossm-about.html

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
