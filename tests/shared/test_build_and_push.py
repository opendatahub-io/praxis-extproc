#!/usr/bin/env python3
"""Static safety tests for source-locked image builds and registry pushes."""

from pathlib import Path
import unittest


SCRIPT = Path(__file__).with_name("build_and_push.sh").read_text(encoding="utf-8")
README = (Path(__file__).parents[1] / "grid-llmd-e2e" / "KIND.md").read_text(encoding="utf-8")


class BuildAndPushTests(unittest.TestCase):
    def test_e2e_docs_map_each_component_to_its_source_containerfile(self) -> None:
        self.assertIn("defaults `DOCKERFILE` to `Containerfile`", README)
        for row in (
            "| Grid Gateway | `grid` | `deploy/gateway/Containerfile` |",
            "| Grid operator | `grid-operator` | `deploy/operator/Containerfile` |",
            "| Praxis provider proxy | `praxis-ai` | `Containerfile` |",
            "| Praxis ExtProc | `extproc` | `Containerfile` |",
            "| EPP | `epp` | `Dockerfile.epp` |",
        ):
            with self.subTest(row=row):
                self.assertIn(row, README)

    def test_registry_tls_is_secure_by_default_and_dev_bypass_is_explicit(self) -> None:
        self.assertIn('IMAGE_PUSH_TLS_VERIFY="${IMAGE_PUSH_TLS_VERIFY:-true}"', SCRIPT)
        self.assertIn('("$CLUSTER_ENV" == ephemeral-dev || "$CLUSTER_ENV" == kind)', SCRIPT)
        self.assertIn('"$ALLOW_INSECURE_IMAGE_PUSH" == true', SCRIPT)
        self.assertIn('push_args+=(--tls-verify=false)', SCRIPT)

    def test_docker_push_uses_skopeo_digest_file_and_explicit_destination_policy(self) -> None:
        docker_branch = SCRIPT.split('if [[ "$CONTAINER_ENGINE" == docker ]]', 1)[1].split('\nelse\n', 1)[0]
        self.assertIn('"$CONTAINER_ENGINE" push "$tag"', docker_branch)
        self.assertIn("sed -nE 's/.*digest: (sha256:[0-9a-f]{64}).*/\\1/p'", docker_branch)
        self.assertIn('DOCKER_CONFIG="$docker_config_dir"', docker_branch)
        self.assertIn('REGISTRY_CERT_DIR is not supported by CONTAINER_ENGINE=docker', docker_branch)

    def test_registry_credentials_are_file_based_and_excluded_from_evidence(self) -> None:
        self.assertIn('REGISTRY_AUTH_FILE="${REGISTRY_AUTH_FILE:-}"', SCRIPT)
        self.assertIn('REGISTRY_AUTH_FILE must refer to a file outside evidence', SCRIPT)
        self.assertIn('push_args+=(--authfile "$REGISTRY_AUTH_FILE")', SCRIPT)
        self.assertIn('inspect_args+=(--authfile "$REGISTRY_AUTH_FILE")', SCRIPT)
        self.assertIn('"credentials_written_to_evidence": False', SCRIPT)

    def test_grid_gateway_embeds_the_resolved_source_revision(self) -> None:
        self.assertIn('if [[ "$COMPONENT" == grid-gateway || "$COMPONENT" == grid-operator ]]', SCRIPT)
        self.assertIn('SOURCE_COMPONENT="${SOURCE_COMPONENT:-$COMPONENT}"', SCRIPT)
        self.assertIn('SOURCE_LOCKS_FILE" "$SOURCE_COMPONENT"', SCRIPT)
        self.assertIn('--build-arg "GRID_GIT_COMMIT=$SOURCE_SHA"', SCRIPT)
        self.assertIn('--build-arg GRID_GIT_TREE_STATE=clean', SCRIPT)

    def test_grid_operator_embeds_the_resolved_source_revision(self) -> None:
        self.assertIn('if [[ "$COMPONENT" == grid-gateway || "$COMPONENT" == grid-operator ]]', SCRIPT)
        self.assertIn('--build-arg "GRID_GIT_COMMIT=$SOURCE_SHA"', SCRIPT)
        self.assertIn('--build-arg "GRID_GIT_VERSION=$version"', SCRIPT)
        self.assertIn('--build-arg "GRID_BUILD_DATE=$(date -u +%Y%m%d)"', SCRIPT)

    def test_epp_embeds_the_resolved_source_revision(self) -> None:
        self.assertIn('if [[ "$COMPONENT" == epp ]]', SCRIPT)
        self.assertIn('--build-arg "COMMIT_SHA=$SOURCE_SHA"', SCRIPT)

    def test_extproc_e2e_feature_is_an_explicit_build_input(self) -> None:
        self.assertIn('EXT_PROC_CARGO_FEATURES="${EXT_PROC_CARGO_FEATURES:-}"', SCRIPT)
        self.assertIn('"$COMPONENT" == extproc', SCRIPT)
        self.assertIn('build_args+=(--build-arg "CARGO_FEATURES=$EXT_PROC_CARGO_FEATURES")', SCRIPT)
        self.assertIn('"extproc_cargo_features": os.environ.get("EXT_PROC_CARGO_FEATURES") or None', SCRIPT)
        self.assertIn('EXT_PROC_CARGO_FEATURES must be a comma-separated list of Cargo feature names', SCRIPT)

    def test_push_tag_is_not_the_deployed_reference(self) -> None:
        self.assertIn('tag="$IMAGE_REPOSITORY:e2e-$RUN_ID-${SOURCE_SHA:0:12}"', SCRIPT)
        self.assertIn('push_args+=(--cert-dir "$REGISTRY_CERT_DIR/${push_repository%%/*}")', SCRIPT)
        self.assertIn('inspect_args+=(--cert-dir "$REGISTRY_CERT_DIR/${push_repository%%/*}")', SCRIPT)
        self.assertIn('image_ref="$deploy_repository@$digest"', SCRIPT)
        self.assertIn('inspect_ref="$push_repository@$digest"', SCRIPT)
        self.assertIn('"$CONTAINER_ENGINE" "${push_args[@]}" "$tag"', SCRIPT)

    def test_build_helper_emits_one_machine_readable_deploy_reference(self) -> None:
        self.assertIn('--output "$EVIDENCE_DIR/image-provenance-$COMPONENT.json"', SCRIPT)
        self.assertIn('python3 "$SCRIPT_DIR/verify_image_provenance.py" "${verify_args[@]}" >&2', SCRIPT)
        self.assertIn('printf \'%s\\n\' "$image_ref"', SCRIPT)

    def test_oci_source_label_can_use_canonical_url_separate_from_git_remote(self) -> None:
        self.assertIn('OCI_SOURCE_URL="${OCI_SOURCE_URL:-}"', SCRIPT)
        self.assertIn('if [[ -z "$OCI_SOURCE_URL" ]]; then OCI_SOURCE_URL="$SOURCE_URL"; fi', SCRIPT)
        self.assertIn('--label "org.opencontainers.image.source=$OCI_SOURCE_URL"', SCRIPT)
        self.assertIn('--source-url "$OCI_SOURCE_URL"', SCRIPT)
        self.assertIn('"oci_source_url": oci_source', SCRIPT)

    def test_compatibility_patch_is_exactly_locked_and_visible_in_provenance(self) -> None:
        self.assertIn('SOURCE_PATCH_FILE="${SOURCE_PATCH_FILE:-}"', SCRIPT)
        self.assertIn('record.get("source_patch_sha256") or "-"', SCRIPT)
        self.assertIn('record.get("patched_build_context_sha256", "")', SCRIPT)
        self.assertIn('git -C "$SOURCE_WORKTREE" diff --binary --full-index "$SOURCE_SHA"', SCRIPT)
        self.assertIn('actual_diff_sha256" == "$LOCKED_SOURCE_PATCH_SHA256"', SCRIPT)
        self.assertIn('org.praxis.source-patch.sha256=$source_patch_sha256', SCRIPT)
        self.assertIn('--source-patch-sha256 "$source_patch_sha256"', SCRIPT)

    def test_component_subdirectory_build_context_is_locked_and_recorded(self) -> None:
        self.assertIn('BUILD_CONTEXT_SUBDIR="${BUILD_CONTEXT_SUBDIR:-}"', SCRIPT)
        self.assertIn('git -C "$SOURCE_WORKTREE" archive --format=tar "$SOURCE_SHA" "$BUILD_CONTEXT_SUBDIR"', SCRIPT)
        self.assertIn('--label "org.praxis.build-context.subdir=$BUILD_CONTEXT_SUBDIR"', SCRIPT)
        self.assertIn('"source_lock_build_context_sha256": source_lock_context', SCRIPT)
        self.assertIn('"build_context_subdir": context_subdir or None', SCRIPT)
        self.assertIn('BUILD_CONTEXT_SUBDIR cannot be combined with a patched source lock', SCRIPT)

    def test_optional_build_limits_are_enforced_and_recorded(self) -> None:
        self.assertIn('CONTAINER_BUILD_JOBS="${CONTAINER_BUILD_JOBS:-}"', SCRIPT)
        self.assertIn('build_args+=(--jobs "$CONTAINER_BUILD_JOBS")', SCRIPT)
        self.assertIn('build_args+=(--cpu-quota "$CONTAINER_BUILD_CPU_QUOTA" --cpu-period "$CONTAINER_BUILD_CPU_PERIOD")', SCRIPT)
        self.assertIn('build_args+=(--memory "$CONTAINER_BUILD_MEMORY")', SCRIPT)
        self.assertIn('resource-limited container builds require CONTAINER_ENGINE=podman', SCRIPT)
        self.assertIn('"container_build_limits"', SCRIPT)

    def test_kind_four_cpu_profile_is_additive_and_keeps_full_profile(self) -> None:
        create = (Path(__file__).parents[1] / "grid-llmd-e2e" / "create-kind-environment.sh").read_text(encoding="utf-8")
        build = (Path(__file__).parents[1] / "grid-llmd-e2e" / "build-kind-images.sh").read_text(encoding="utf-8")
        self.assertIn('full)', create)
        self.assertIn('KIND_MIN_CPU_COUNT:-8', create)
        self.assertIn('gha-standard-4cpu)', create)
        self.assertIn('EXACT_CPU_COUNT=4', create)
        self.assertIn('CONTAINER_BUILD_JOBS=1', build)
        self.assertIn('CONTAINER_BUILD_CPU_QUOTA=400000', build)
        self.assertIn('CONTAINER_BUILD_MEMORY=6g', build)

    def test_provenance_verifier_rejects_unrecorded_patch_labels(self) -> None:
        verifier = Path(__file__).with_name("verify_image_provenance.py").read_text(encoding="utf-8")
        self.assertIn("elif patch_label is not None:", verifier)
        self.assertIn('source_patch_sha256 = getattr(args, "source_patch_sha256", "")', verifier)
        self.assertIn('result["source_patch_sha256"] = source_patch_sha256', verifier)


if __name__ == "__main__":
    unittest.main()
