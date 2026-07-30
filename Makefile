.PHONY: all build release check clean \
	test test-integration lint fmt doc audit \
	coverage-check \
	require-container-engine \
	container container-release images kind-up kind-down smoke-test \
	dev-env dev-push dev-integration \
	setup-hooks \
	help

# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------

CONTAINER_ENGINE  ?= $(shell command -v podman 2>/dev/null || command -v docker 2>/dev/null)
V                 ?=
KIND_CLUSTER_NAME ?= praxis-extproc
EXTPROC_IMAGE     ?= praxis-extproc:dev
KUBECTL           ?= kubectl --context kind-$(KIND_CLUSTER_NAME)

ifneq ($(V),)
  _NOCAPTURE := -- --nocapture
endif

# ---------------------------------------------------------------------------
# All
# ---------------------------------------------------------------------------

all: build fmt lint test audit

# ---------------------------------------------------------------------------
# Build
# ---------------------------------------------------------------------------

build:
	cargo build

release:
	cargo build --release

check:
	cargo check

clean:
	cargo clean
	rm -rf .container

# ---------------------------------------------------------------------------
# Test
# ---------------------------------------------------------------------------

test:
	cargo test $(_NOCAPTURE)

test-integration:
	cargo test --features integration -- --ignored $(if $(V),--nocapture,)

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

coverage-check:
	cargo llvm-cov --fail-under-lines 80

# ---------------------------------------------------------------------------
# Container
# ---------------------------------------------------------------------------

require-container-engine:
ifndef CONTAINER_ENGINE
	$(error No container engine found. Install podman or docker)
endif

container: build | require-container-engine
	mkdir -p .container
	cp target/debug/praxis-extproc .container/praxis-extproc
	$(CONTAINER_ENGINE) build -t $(EXTPROC_IMAGE) -f Containerfile .

container-release: release | require-container-engine
	mkdir -p .container
	cp target/release/praxis-extproc .container/praxis-extproc
	strip .container/praxis-extproc
	$(CONTAINER_ENGINE) build -t $(EXTPROC_IMAGE) -f Containerfile .

images: container-release

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

dev-push: container-release
	kind load docker-image $(EXTPROC_IMAGE) --name $(KIND_CLUSTER_NAME)
	$(KUBECTL) -n praxis-extproc rollout restart deployment/praxis-extproc
	$(KUBECTL) -n praxis-extproc rollout status deployment/praxis-extproc --timeout=120s

dev-integration:
	@kind get kubeconfig --name $(KIND_CLUSTER_NAME) > /tmp/kind-$(KIND_CLUSTER_NAME).kubeconfig
	KUBECONFIG=/tmp/kind-$(KIND_CLUSTER_NAME).kubeconfig \
	cargo test --features integration -- --ignored $(if $(V),--nocapture,)

# ---------------------------------------------------------------------------
# Dev Setup
# ---------------------------------------------------------------------------

setup-hooks:
	@ln -sf ../../.hooks/pre-commit .git/hooks/pre-commit
	@echo "Git hooks installed"

# ---------------------------------------------------------------------------
# Help
# ---------------------------------------------------------------------------

help:
	@echo "Variables:"
	@echo "  V=1                show test output (--nocapture)"
	@echo "  CONTAINER_ENGINE   container runtime (auto-detected)"
	@echo "  KIND_CLUSTER_NAME  KIND cluster name (default: praxis-extproc)"
	@echo "  EXTPROC_IMAGE      container image tag (default: praxis-extproc:dev)"
	@echo ""
	@echo "Top-level:"
	@echo "  all              build + lint + test + audit"
	@echo ""
	@echo "Build:"
	@echo "  build            cargo build"
	@echo "  release          cargo build --release"
	@echo "  check            cargo check"
	@echo "  clean            cargo clean"
	@echo ""
	@echo "Test:"
	@echo "  test             run all tests"
	@echo "  test-integration run integration tests (ignored tests)"
	@echo ""
	@echo "Quality:"
	@echo "  lint             clippy + rustfmt check"
	@echo "  fmt              format with nightly rustfmt"
	@echo "  doc              build docs with warnings denied"
	@echo "  audit            cargo audit + cargo deny"
	@echo "  coverage-check   fail if line coverage < 80%%"
	@echo ""
	@echo "Container:"
	@echo "  container         build debug container image"
	@echo "  container-release build release container image"
	@echo "  images            alias for container-release"
	@echo ""
	@echo "KIND:"
	@echo "  kind-up          create cluster + deploy"
	@echo "  kind-down        delete cluster"
	@echo "  smoke-test       run smoke tests against cluster"
	@echo ""
	@echo "Dev Setup:"
	@echo "  setup-hooks      install git pre-commit hook"
	@echo ""
	@echo "Development:"
	@echo "  dev-env          create/reuse persistent cluster"
	@echo "  dev-push         build + load + rollout"
	@echo "  dev-integration  run integration tests against cluster"
