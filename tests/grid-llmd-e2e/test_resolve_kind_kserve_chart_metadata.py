#!/usr/bin/env python3

import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
from resolve_kind_kserve_chart_metadata import CHARTS, resolve_chart_metadata  # noqa: E402


class ResolveKindKServeChartMetadataTests(unittest.TestCase):
    def make_source(self, root: Path, versions=None, names=None):
        versions = versions or {}
        names = names or {}
        for chart_name, relative_path in CHARTS.items():
            chart_file = root / relative_path
            chart_file.parent.mkdir(parents=True)
            version = versions.get(chart_name, "v0.21.0-rc1")
            name = names.get(chart_name, chart_name)
            chart_file.write_text(
                f"apiVersion: v2\nname: {name}\nversion: {version}\nappVersion: {version}\n",
                encoding="utf-8",
            )
        values = root / "charts/kserve-runtime-configs/values.yaml"
        values.write_text("kserve:\n  version: v0.21.0-rc1\n", encoding="utf-8")

    def test_accepts_locked_release_candidate_chart_versions(self):
        with tempfile.TemporaryDirectory() as temporary:
            source = Path(temporary)
            self.make_source(source)
            metadata = resolve_chart_metadata(source, "a" * 40)

        self.assertEqual(metadata["source_sha"], "a" * 40)
        self.assertEqual(
            {chart["version"] for chart in metadata["charts"].values()},
            {"v0.21.0-rc1"},
        )
        self.assertEqual(len(metadata["runtime_values_sha256"]), 64)
        self.assertTrue(
            all(len(chart["chart_yaml_sha256"]) == 64 for chart in metadata["charts"].values())
        )

    def test_allows_chart_packages_to_have_source_declared_versions(self):
        with tempfile.TemporaryDirectory() as temporary:
            source = Path(temporary)
            self.make_source(
                source,
                versions={
                    "kserve-llmisvc-crd": "v0.21.0-rc1",
                    "kserve-llmisvc-resources": "v0.21.0-rc1",
                    "kserve-runtime-configs": "v0.21.0-rc2",
                },
            )
            metadata = resolve_chart_metadata(source, "b" * 40)

        self.assertEqual(
            metadata["charts"]["kserve-runtime-configs"]["version"], "v0.21.0-rc2"
        )

    def test_rejects_wrong_chart_identity(self):
        with tempfile.TemporaryDirectory() as temporary:
            source = Path(temporary)
            self.make_source(source, names={"kserve-llmisvc-crd": "other-chart"})
            with self.assertRaisesRegex(ValueError, "expected chart kserve-llmisvc-crd"):
                resolve_chart_metadata(source, "c" * 40)

    def test_rejects_malformed_chart_version(self):
        with tempfile.TemporaryDirectory() as temporary:
            source = Path(temporary)
            self.make_source(source, versions={"kserve-llmisvc-crd": "latest"})
            with self.assertRaisesRegex(ValueError, "chart version is malformed"):
                resolve_chart_metadata(source, "d" * 40)

    def test_rejects_abbreviated_source_sha(self):
        with tempfile.TemporaryDirectory() as temporary:
            source = Path(temporary)
            self.make_source(source)
            with self.assertRaisesRegex(ValueError, "full lowercase Git SHA"):
                resolve_chart_metadata(source, "deadbeef")


if __name__ == "__main__":
    unittest.main()
