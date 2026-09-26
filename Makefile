.PHONY: all build release check clean \
	test test-integration lint fmt doc audit \
	coverage-check \
	require-container-engine require-podman require-go require-oc \
	container container-release images kind-up kind-down smoke-test \
	build-fips release-fips check-fips lint-fips test-fips \
	fips-check fips-deps fips-report fips-signature-store fips-verify-image \
	fips-scan fips-scanner fips-smoke \
	dev-env dev-push dev-integration \
	manifests-demo manifests-odh manifests-openshift \
	e2e-setup e2e-teardown e2e-test \
	setup-hooks \
	help

# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------

CONTAINER_ENGINE  ?= $(shell command -v podman 2>/dev/null || command -v docker 2>/dev/null)
V                 ?=
KIND_CLUSTER_NAME ?= praxis-extproc
# Fully-qualified: podman tags local builds `localhost/...`, which won't match
# the `docker.io/library/...` Kubernetes resolves to under `imagePullPolicy: Never`.
EXTPROC_IMAGE     ?= docker.io/library/praxis-extproc:dev
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

# ---------------------------------------------------------------------------
# Test
# ---------------------------------------------------------------------------

test:
	cargo test --workspace $(_NOCAPTURE)

test-integration:
	cargo test --features integration -- --ignored $(if $(V),--nocapture,)

# ---------------------------------------------------------------------------
# Quality
# ---------------------------------------------------------------------------

lint:
	cargo clippy --workspace --all-targets -- -D warnings
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

# The product image is the FIPS build: the Containerfile's defaults are
# FIPS_FEATURES and the digest-pinned UBI 9 bases, passed explicitly here so
# the Makefile stays the single place they are defined. With podman the bases'
# Red Hat signatures are verified first; docker cannot verify them (Konflux
# builds this same file under its own provenance checks).
_VERIFY_BASES := $(if $(filter podman,$(notdir $(CONTAINER_ENGINE))),fips-verify-image,)

container: $(_VERIFY_BASES) | require-container-engine
	$(CONTAINER_ENGINE) build \
		--no-cache \
		--build-arg CARGO_PROFILE=debug \
		$(FIPS_BUILD_ARGS) \
		-t $(EXTPROC_IMAGE) \
		-f Containerfile \
		.

container-release: $(_VERIFY_BASES) | require-container-engine
	$(CONTAINER_ENGINE) build \
		--build-arg CARGO_PROFILE=release \
		$(FIPS_BUILD_ARGS) \
		-t $(EXTPROC_IMAGE) \
		-f Containerfile \
		.

images: container-release

# ---------------------------------------------------------------------------
# FIPS
# ---------------------------------------------------------------------------
#
# The product image is the FIPS build (see the Containerfile). It leaves out
# what is known to carry pure-Rust cryptography, so nobody has to know which
# features to pick:
#
#   policy-engine     the praxis policy filter's JWT, OAuth and Valkey
#                     plugins carry aws-lc-rs, sha2 and hmac
#   responses-store   the Responses store is built on sqlx, whose migration
#                     checksums use sha2
#
# The Responses filters (responses) and the SigV4 signer (aws-sigv4, which
# signs through the system OpenSSL since praxis-ai moved it off sha2/hmac)
# stay in. FIPS_FEATURES is the single place this is defined; the
# Containerfile's CARGO_FEATURES default mirrors it and must be kept in sync.
#
# The local FIPS build goes to its own target directory so it never
# overwrites, or is mistaken for, the default build.
#
#   make build-fips        FIPS build, debug profile
#   make release-fips      FIPS build, release profile, with the crate manifest
#   make check-fips        cargo check of the FIPS build
#   make lint-fips         clippy + rustfmt for the FIPS feature set
#   make test-fips         tests for the FIPS feature set
#   make fips-check        build on UBI 9 and print the compliance report
#   make fips-report       the same report against the local FIPS build
#   make fips-deps         dependency graph only (seconds, no build)
#   make fips-smoke        run the image once (validates the example config)
#   make fips-scan         run Red Hat's scanner (check-payload) on the image,
#                          warnings fatal: the actual gate
#   make fips-scanner      build check-payload at the pinned revision
#   make fips-verify-image verify the pinned UBI 9 bases are Red Hat's
#   make fips-signature-store
#                          point podman at Red Hat's signature store; needed
#                          once on Debian/Ubuntu hosts, a no-op elsewhere
#
# The report, the image verification and the signature-store setup are
# `cargo xtask fips` commands (xtask/src/fips/). See docs/fips.md.

FIPS_FEATURES           := responses,aws-sigv4
FIPS_TARGET_DIR         := target/fips
FIPS_BIN                ?= $(FIPS_TARGET_DIR)/release/praxis-extproc
FIPS_CARGO_ARGS         := -p praxis-extproc --no-default-features --features $(FIPS_FEATURES) --target-dir $(FIPS_TARGET_DIR)
# Red Hat's scanner reads the crate list that `cargo auditable` embeds in the
# binary (the .dep-v0 section); without it a binary is graded inconclusive.
# `make release-fips` embeds it when cargo-auditable is installed (`cargo
# install cargo-auditable --version 0.7.6 --locked`); the report says so when
# it was not.
#
# The list must be exactly the crates compiled in. On a stable toolchain
# cargo-auditable derives it from `cargo metadata`, which unifies features
# across the whole workspace and activates weak features (`dep?/feature`)
# the real build never turns on; with rustls that puts `ring` in the manifest
# of a binary that never compiled it, and the scanner fails on the name alone.
# Cargo's SBOM precursor (`-Zsbom`, unstable) is the exact list, so the
# release build enables it: RUSTC_BOOTSTRAP=1 lets stable cargo accept the
# flag, and the env overrides hand rustc and every build script
# RUSTC_BOOTSTRAP=-1, which forbids unstable features, so the code compiled is
# the stable code. Drop this once cargo's `build.sbom` is stable
# (rust-lang/cargo#13709). Same recipe in the Containerfile.
CARGO_AUDITABLE         := $(shell command -v cargo-auditable >/dev/null 2>&1 && echo "cargo auditable" || echo "cargo")
FIPS_SBOM_ENV           := RUSTC_BOOTSTRAP=1 CARGO_BUILD_SBOM=true
FIPS_SBOM_ARGS          := -Zsbom --config 'env.RUSTC_BOOTSTRAP.value="-1"' --config 'env.RUSTC_BOOTSTRAP.force=true'
FIPS_UBI9_DIGEST        := sha256:a4b9ec09b1e790a53ef25b7777c539976abe519248264298e5194dcbceac8c31
FIPS_UBI9_MINIMAL_DIGEST := sha256:8ebe2ad8fdf3cab3e5a53c1edc69194c98209cfadab24b884f4ad9ebcf7bbbfc
FIPS_UBI9_IMAGE         := registry.access.redhat.com/ubi9/ubi@$(FIPS_UBI9_DIGEST)
FIPS_UBI9_MINIMAL_IMAGE := registry.access.redhat.com/ubi9/ubi-minimal@$(FIPS_UBI9_MINIMAL_DIGEST)
FIPS_CHECK_IMAGE        ?= praxis-extproc-fips-check
FIPS_BUILD_ARGS         := --build-arg UBI9_DIGEST=$(FIPS_UBI9_DIGEST) \
	--build-arg UBI9_MINIMAL_DIGEST=$(FIPS_UBI9_MINIMAL_DIGEST) \
	--build-arg CARGO_FEATURES=$(FIPS_FEATURES)
XTASK                   := cargo run -q -p xtask --
# Red Hat's scanner, openshift/check-payload, at the revision that added Rust
# support (the head of its PR #360, fetched by commit so a rewrite of the PR
# cannot break the build). `make fips-scanner` builds it into target/fips;
# point CHECK_PAYLOAD at another build to use it instead.
CHECK_PAYLOAD_REPO      := https://github.com/openshift/check-payload
CHECK_PAYLOAD_REV       := 1ce4e04ed214b98997797ce19a2442f794632e65
CHECK_PAYLOAD_DIR       := $(FIPS_TARGET_DIR)/check-payload
CHECK_PAYLOAD           ?= $(CHECK_PAYLOAD_DIR)/check-payload
# The image as podman's storage names it: a bare name gets podman's implicit
# localhost/ prefix, a registry-qualified EXTPROC_IMAGE does not.
_IMAGE_HEAD             := $(firstword $(subst /, ,$(EXTPROC_IMAGE)))
FIPS_IMAGE_REF          := $(if $(or $(findstring .,$(_IMAGE_HEAD)),$(findstring :,$(_IMAGE_HEAD)),$(filter localhost,$(_IMAGE_HEAD))),$(EXTPROC_IMAGE),localhost/$(EXTPROC_IMAGE))
# The scanner mounts the image from podman's store, which needs the user
# namespace only for rootless podman.
PODMAN_UNSHARE          := $(if $(filter 0,$(shell id -u)),,podman unshare)

require-podman:
	@command -v podman >/dev/null || { echo "podman is required: Red Hat image signatures can only be verified with podman"; exit 1; }

require-go:
	@command -v go >/dev/null || { echo "go is required to build check-payload"; exit 1; }

require-oc:
	@command -v oc >/dev/null || { echo "oc (the OpenShift CLI) is required: check-payload refuses to scan without it on PATH"; exit 1; }

# The debug build is the edit-compile loop; only the release build carries
# the manifest.
build-fips:
	cargo build $(FIPS_CARGO_ARGS)

# cargo before 1.99 does not relink a binary when only the SBOM setting
# changed (rust-lang/cargo#15695, fixed by #17216), so the old binary goes
# first; everything else stays cached. Drop the clean once the toolchains in
# use (here and the UBI rust-toolset) are 1.99 or newer.
release-fips:
ifeq ($(CARGO_AUDITABLE),cargo auditable)
	cargo clean --release -p praxis-extproc --target-dir $(FIPS_TARGET_DIR)
	$(FIPS_SBOM_ENV) cargo auditable $(FIPS_SBOM_ARGS) build --release $(FIPS_CARGO_ARGS)
else
	@echo "warning: cargo-auditable is not installed; no crate manifest will be embedded (cargo install cargo-auditable --version 0.7.6 --locked)"
	cargo build --release $(FIPS_CARGO_ARGS)
endif

check-fips:
	cargo check $(FIPS_CARGO_ARGS)

# Clippy over every target of the FIPS build, plus the rustfmt check (which
# is feature-independent but belongs in "is the FIPS version clean").
lint-fips:
	cargo clippy $(FIPS_CARGO_ARGS) --all-targets -- -D warnings
	cargo +nightly fmt --all -- --check

# The tests resolved exactly as the FIPS build resolves them.
test-fips:
	cargo test $(FIPS_CARGO_ARGS) $(_NOCAPTURE)

# podman finds Red Hat's detached image signatures through its registries.d
# (containers-registries.d(5)). Fedora and RHEL ship the entry; Debian and
# Ubuntu, GitHub's runners included, ship no registries.d at all, and then
# every Red Hat image looks unsigned. This installs the bundled entry for the
# current user when the registries.d podman reads names none, and does
# nothing otherwise. CI runs it before fips-verify-image.
fips-signature-store:
	$(XTASK) fips signature-store --install

fips-verify-image: | require-podman
	$(XTASK) fips verify-image --pinned-in Containerfile $(FIPS_UBI9_IMAGE)
	$(XTASK) fips verify-image --pinned-in Containerfile $(FIPS_UBI9_MINIMAL_IMAGE)

# The binary starts on ubi-minimal, installs the OpenSSL provider, logs the
# FIPS signals and accepts the example config; a cheap proof that the image
# runs before the scan.
fips-smoke: | require-podman
	podman run --rm -v $(CURDIR)/examples/praxis-extproc.yaml:/etc/praxis/extproc.yaml:ro,z \
		$(EXTPROC_IMAGE) --validate -c /etc/praxis/extproc.yaml

fips-check: fips-verify-image
	podman build -f Containerfile --target report $(FIPS_BUILD_ARGS) \
		-t $(FIPS_CHECK_IMAGE) .
	podman run --rm $(FIPS_CHECK_IMAGE)

fips-report:
	$(XTASK) fips report --features $(FIPS_FEATURES) $(FIPS_BIN)

# The graph check is `cargo xtask fips report` (cargo tree scoped to the
# binary and its feature set) rather than cargo-deny, which resolves features
# workspace-wide and cannot see the FIPS build's real graph.
fips-deps:
	$(XTASK) fips report --deps-only --features $(FIPS_FEATURES)

# --fail-on-warnings makes an inconclusive verdict (for example a binary
# without a crate manifest) fail, as Red Hat's gated scans do. Needs a Linux
# podman (rootless or root), not a podman machine, and the OpenShift CLI on
# PATH: check-payload refuses to start without it.
fips-scan: | require-podman require-oc
	@[ -x "$(CHECK_PAYLOAD)" ] || { echo "check-payload not found at $(CHECK_PAYLOAD): run 'make fips-scanner' (needs go) or set CHECK_PAYLOAD"; exit 1; }
	$(PODMAN_UNSHARE) $(CHECK_PAYLOAD) scan image \
		--spec containers-storage:$(FIPS_IMAGE_REF) --fail-on-warnings

# Built as upstream builds it (CGO_ENABLED=0, vendored modules).
fips-scanner: | require-go
	@mkdir -p $(CHECK_PAYLOAD_DIR)
	@[ -d $(CHECK_PAYLOAD_DIR)/.git ] || git -C $(CHECK_PAYLOAD_DIR) init --quiet
	git -C $(CHECK_PAYLOAD_DIR) fetch --quiet --depth 1 $(CHECK_PAYLOAD_REPO) $(CHECK_PAYLOAD_REV)
	git -C $(CHECK_PAYLOAD_DIR) checkout --quiet FETCH_HEAD
	cd $(CHECK_PAYLOAD_DIR) && CGO_ENABLED=0 go build -o check-payload .

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
# E2E (Forge)
# ---------------------------------------------------------------------------

FORGE_BIN    ?= praxis-forge
FORGE_CONFIG := forge.yaml
INFERENCE_SIM_IMAGE ?= ghcr.io/llm-d/llm-d-inference-sim:v0.8.2
FORGE_CMD = "$(FORGE_BIN)" --config "$(FORGE_CONFIG)" --runtime "$(notdir $(CONTAINER_ENGINE))"

e2e-setup: images
	$(FORGE_CMD) cluster create e2e
	$(FORGE_CMD) cluster load-image e2e "$(EXTPROC_IMAGE)"
	$(FORGE_CMD) stack apply e2e

e2e-teardown:
	$(FORGE_CMD) cluster delete e2e

e2e-test:
	bash hack/scripts/e2e-test.sh $(if $(V),-- --nocapture,)

# ---------------------------------------------------------------------------
# Iterative Development
# ---------------------------------------------------------------------------

dev-env: images
	KIND_CLUSTER_NAME=$(KIND_CLUSTER_NAME) \
	EXTPROC_IMAGE=$(EXTPROC_IMAGE) \
	bash hack/setup-kind.sh

dev-push: container-release
	kind load docker-image $(EXTPROC_IMAGE) --name $(KIND_CLUSTER_NAME)
	$(KUBECTL) -n praxis-extproc rollout restart deployment/payload-processing
	$(KUBECTL) -n praxis-extproc rollout status deployment/payload-processing --timeout=120s

dev-integration:
	@kind get kubeconfig --name $(KIND_CLUSTER_NAME) > /tmp/kind-$(KIND_CLUSTER_NAME).kubeconfig
	KUBECONFIG=/tmp/kind-$(KIND_CLUSTER_NAME).kubeconfig \
	cargo test --features integration -- --ignored $(if $(V),--nocapture,)

manifests-demo:
	@kubectl kustomize deploy/overlays/demo

manifests-odh:
	@kubectl kustomize deploy/overlays/odh

manifests-openshift:
	@kubectl kustomize deploy/overlays/openshift

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
	@echo "  EXTPROC_IMAGE      container image tag (default: docker.io/library/praxis-extproc:dev)"
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
	@echo "  test             run all tests (workspace)"
	@echo "  test-integration run integration tests (ignored tests)"
	@echo ""
	@echo "Quality:"
	@echo "  lint             clippy (workspace) + rustfmt check"
	@echo "  fmt              format with nightly rustfmt"
	@echo "  doc              build docs with warnings denied"
	@echo "  audit            cargo audit + cargo deny"
	@echo "  coverage-check   fail if line coverage < 80%%"
	@echo ""
	@echo "Container (the product image is the FIPS build on UBI 9):"
	@echo "  container         debug image (in-container cargo)"
	@echo "  container-release release image (in-container cargo, embedded crate manifest)"
	@echo "  images            alias for container-release"
	@echo ""
	@echo "FIPS:"
	@echo "  build-fips           FIPS build, debug profile, into target/fips"
	@echo "  release-fips         FIPS build, release profile, into target/fips, with the embedded crate manifest"
	@echo "  check-fips           cargo check of the FIPS build"
	@echo "  lint-fips            clippy (all targets) + rustfmt check for the FIPS feature set"
	@echo "  test-fips            tests resolved as the FIPS build"
	@echo "  fips-check           build on UBI 9 and print the compliance report (fails while findings remain)"
	@echo "  fips-report          compliance report against the local FIPS build (FIPS_BIN=target/fips/release/praxis-extproc)"
	@echo "  fips-deps            dependency graph vs Red Hat's crypto denylist (seconds, no build)"
	@echo "  fips-smoke           run the image once to validate the example config"
	@echo "  fips-scan            run Red Hat's scanner (check-payload) on the image, warnings fatal"
	@echo "  fips-scanner         build check-payload at the pinned revision into target/fips (needs go)"
	@echo "  fips-verify-image    verify the pinned UBI 9 base images are Red Hat's (digest + signature)"
	@echo "  fips-signature-store point podman at Red Hat's signature store (once, on Debian/Ubuntu hosts)"
	@echo ""
	@echo "KIND:"
	@echo "  kind-up          create cluster + deploy"
	@echo "  kind-down        delete cluster"
	@echo "  smoke-test       run smoke tests against cluster"
	@echo ""
	@echo "Manifests:"
	@echo "  manifests-demo      kubectl kustomize deploy/overlays/demo"
	@echo "  manifests-odh       kubectl kustomize deploy/overlays/odh"
	@echo "  manifests-openshift kubectl kustomize deploy/overlays/openshift"
	@echo ""
	@echo "E2E (Forge):"
	@echo "  e2e-setup        create Kind cluster + install all stacks"
	@echo "  e2e-teardown     delete Kind e2e cluster"
	@echo "  e2e-test         run k8s e2e tests against cluster"
	@echo ""
	@echo "Dev Setup:"
	@echo "  setup-hooks      install git pre-commit hook"
	@echo ""
	@echo "Development:"
	@echo "  dev-env          create/reuse persistent cluster"
	@echo "  dev-push         build + load + rollout"
	@echo "  dev-integration  run integration tests against cluster"
