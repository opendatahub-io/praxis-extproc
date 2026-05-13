.PHONY: all build release test lint fmt doc audit clean
.PHONY: images container kind-up kind-down smoke-test test-integration
.PHONY: dev-env dev-push dev-integration

# ---------------------------------------------------------------------------
# Environment
# ---------------------------------------------------------------------------

KIND_CLUSTER_NAME ?= praxis-extproc
EXTPROC_IMAGE    ?= praxis-extproc:dev
KUBECTL          ?= kubectl --context kind-$(KIND_CLUSTER_NAME)

# ---------------------------------------------------------------------------
# Build
# ---------------------------------------------------------------------------

all: build fmt lint test audit

build:
	cargo build

release:
	cargo build --release

# ---------------------------------------------------------------------------
# Quality
# ---------------------------------------------------------------------------

lint:
	cargo clippy --all-targets -- -D warnings
	cargo +nightly fmt --all -- --check

fmt:
	cargo +nightly fmt --all

doc:
	RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --document-private-items

audit:
	cargo audit
	cargo deny check

clean:
	cargo clean

# ---------------------------------------------------------------------------
# Test
# ---------------------------------------------------------------------------

test:
	cargo test

test-integration:
	cargo test --features integration -- --ignored $(if $(V),--nocapture,)

# ---------------------------------------------------------------------------
# Container
# ---------------------------------------------------------------------------

container:
	podman build -t $(EXTPROC_IMAGE) -f Containerfile . || \
	docker build -t $(EXTPROC_IMAGE) -f Containerfile .

images:
	docker build -t $(EXTPROC_IMAGE) -f Containerfile .

# ---------------------------------------------------------------------------
# KIND
# ---------------------------------------------------------------------------

kind-up: images
	KIND_CLUSTER_NAME=$(KIND_CLUSTER_NAME) \
	EXTPROC_IMAGE=$(EXTPROC_IMAGE) \
	bash hack/setup-kind.sh

kind-down:
	KIND_CLUSTER_NAME=$(KIND_CLUSTER_NAME) \
	bash hack/teardown-kind.sh

smoke-test:
	KIND_CLUSTER_NAME=$(KIND_CLUSTER_NAME) \
	bash hack/smoke-test.sh

# ---------------------------------------------------------------------------
# Iterative Development
# ---------------------------------------------------------------------------

dev-env: images
	KIND_CLUSTER_NAME=$(KIND_CLUSTER_NAME) \
	EXTPROC_IMAGE=$(EXTPROC_IMAGE) \
	bash hack/setup-kind.sh

dev-push:
	docker build -t $(EXTPROC_IMAGE) -f Containerfile .
	kind load docker-image $(EXTPROC_IMAGE) --name $(KIND_CLUSTER_NAME)
	$(KUBECTL) -n praxis-extproc rollout restart deployment/praxis-extproc
	$(KUBECTL) -n praxis-extproc rollout status deployment/praxis-extproc --timeout=120s

dev-integration:
	@kind get kubeconfig --name $(KIND_CLUSTER_NAME) > /tmp/kind-$(KIND_CLUSTER_NAME).kubeconfig
	KUBECONFIG=/tmp/kind-$(KIND_CLUSTER_NAME).kubeconfig \
	cargo test --features integration -- --ignored $(if $(V),--nocapture,)
