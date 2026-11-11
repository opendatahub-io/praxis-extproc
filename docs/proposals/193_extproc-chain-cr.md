---
issue: https://github.com/opendatahub-io/praxis-extproc/issues/193
discussion: https://github.com/opendatahub-io/praxis-extproc/issues/193
status: proposed
authors:
  - ishitasequeira
---

# ExtProc chain CR: let Praxis own the rendering of its own deployment

## What?

Give Praxis ExtProc a way to be told what an ExtProc instance should be,
instead of being handed a finished set of Kubernetes resources.

A consumer (today: ai-gateway-controller) creates one small custom resource
per chain per tenant describing **intent** — where the chain sits relative to
authentication, what identity it runs as, whether its metrics endpoint
authenticates scrapes, which config inputs it should load. Praxis consumes
that object, renders the Kubernetes resources its own process needs, and
reports back what it actually created.

```text
consumer: intent ──► ExtProcChain CR ──► Praxis ──► Deployment / Service / config
                             ▲                                │
                             └──── status ◄───────────────────┘
```

### Goals

- A single, schema-validated description of one ExtProc instance, owned by
  the project that knows what an ExtProc instance is.
- Contradictory intent rejected when it is written, not discovered when a pod
  fails to start.
- The project owns the shape of its own configuration file, rather than a
  consumer generating it and the two sides agreeing by convention.
- Addresses that consumers need (gRPC endpoint, metrics endpoint) published
  by the producer instead of reconstructed by the consumer.

## Why?

### Motivation

**A configuration key can take the dataplane down.** `server.metrics_auth`
was written into a generated config before every deployed Praxis build knew
the field. Because `ServerConfig` denies unknown fields, a build that predates
the field rejects the entire document and exits before constructing the gRPC
server. That is not a degraded scrape; it is every tenant on that deployment
losing inference. The build's behaviour is defensible — it was told about a
field it does not have and refused to guess — but the failure mode is
discovered only on a live cluster, by outage.

This is not hypothetical: it happened in the 3.6 cycle, and it is the reason
a consumer pin had to be corrected afterwards.

**The same fact is restated in several unconnected places.** "This chain's pod
holds no Kubernetes API identity" is expressed as a pod-level
`automountServiceAccountToken`, a `metrics_auth` value in a generated config,
the subject of a ClusterRoleBinding, and the credentials on the ServiceMonitor
that scrapes it. Nothing makes those four agree; each was found and fixed
separately, and the fix took a series of PRs because the invariant had no
home. A vendor sync can silently reintroduce any one of them.

**The consumer is string-rewriting manifests it does not own.** Today the
consumer composes a vendored overlay of this project's manifests,
substitutes placeholders, renames every resource per tenant, and generates the
config file itself. Every field this project adds to its own manifests is
therefore a potential break for a program that only wanted to deploy it. The
seam is a manifest bundle, and a manifest bundle is not a contract.

**Knowledge is duplicated across the seam.** The Service name and port appear
both in the Service the consumer applies and in the EnvoyFilter that points at
it, and can only be kept consistent by constructing both from the same
literal. If Praxis published the endpoint it created, the consumer would not
need to know what it is called.

**The image is pinned in several places.** Consumers pin this project's image
in a way the project cannot express or validate, so a build/field mismatch is
only observable at runtime.

### User Stories

- As a Praxis maintainer, I want the shape of the config file to be owned by
  this project, so that adding a field is a one-repo change instead of a
  cross-repo release ordering problem.
- As a Praxis maintainer, I want to be told "what should exist", so that I can
  change how it is deployed without breaking consumers who only rewrite
  manifests.
- As a platform operator, I want a bad combination of settings rejected at
  admission with a message naming the field, so that a mistake cannot become a
  CrashLooping dataplane.
- As a MaaS operator, I want an image upgrade to fail loudly and locally, not
  by taking every tenant's inference down.
- As a consumer author, I want to stop generating and rewriting another
  project's manifests, and to learn the endpoint I need to route to from
  status.

> **Note:** the **How?** section is intentionally omitted from this PR, per
> `docs/proposals.md`. Requirements and design will follow once the direction
> is accepted.
