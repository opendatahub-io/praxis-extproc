#!/usr/bin/env python3
"""Static safety tests for source-locked image builds and registry pushes."""

from pathlib import Path
import unittest


SCRIPT = Path(__file__).with_name("build_and_push.sh").read_text(encoding="utf-8")


class BuildAndPushTests(unittest.TestCase):
    def test_registry_tls_is_secure_by_default_and_dev_bypass_is_explicit(self) -> None:
        self.assertIn('IMAGE_PUSH_TLS_VERIFY="${IMAGE_PUSH_TLS_VERIFY:-true}"', SCRIPT)
        self.assertIn('[[ "$CLUSTER_ENV" == ephemeral-dev && "$ALLOW_INSECURE_IMAGE_PUSH" == true ]]', SCRIPT)
        self.assertIn('push_args+=(--tls-verify=false)', SCRIPT)

    def test_registry_credentials_are_file_based_and_excluded_from_evidence(self) -> None:
        self.assertIn('REGISTRY_AUTH_FILE="${REGISTRY_AUTH_FILE:-}"', SCRIPT)
        self.assertIn('REGISTRY_AUTH_FILE must refer to a file outside evidence', SCRIPT)
        self.assertIn('push_args+=(--authfile "$REGISTRY_AUTH_FILE")', SCRIPT)
        self.assertIn('inspect_args+=(--authfile "$REGISTRY_AUTH_FILE")', SCRIPT)
        self.assertIn('"credentials_written_to_evidence": False', SCRIPT)

    def test_grid_gateway_embeds_the_resolved_source_revision(self) -> None:
        self.assertIn('if [[ "$COMPONENT" == grid-gateway ]]', SCRIPT)
        self.assertIn('SOURCE_COMPONENT="${SOURCE_COMPONENT:-$COMPONENT}"', SCRIPT)
        self.assertIn('SOURCE_LOCKS_FILE" "$SOURCE_COMPONENT"', SCRIPT)
        self.assertIn('--build-arg "GRID_GIT_COMMIT=$SOURCE_SHA"', SCRIPT)
        self.assertIn('--build-arg GRID_GIT_TREE_STATE=clean', SCRIPT)

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
        verifier = SCRIPT.split('python3 "$SCRIPT_DIR/verify_image_provenance.py"', 1)[1]
        self.assertIn('--output "$EVIDENCE_DIR/image-provenance-$COMPONENT.json" >&2', verifier)
        self.assertIn('printf \'%s\\n\' "$image_ref"', SCRIPT)

    def test_oci_source_label_can_use_canonical_url_separate_from_git_remote(self) -> None:
        self.assertIn('OCI_SOURCE_URL="${OCI_SOURCE_URL:-}"', SCRIPT)
        self.assertIn('if [[ -z "$OCI_SOURCE_URL" ]]; then OCI_SOURCE_URL="$SOURCE_URL"; fi', SCRIPT)
        self.assertIn('--label "org.opencontainers.image.source=$OCI_SOURCE_URL"', SCRIPT)
        self.assertIn('--source-url "$OCI_SOURCE_URL"', SCRIPT)
        self.assertIn('"oci_source_url": oci_source', SCRIPT)


if __name__ == "__main__":
    unittest.main()
