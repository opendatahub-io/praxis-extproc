#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

from __future__ import annotations

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from canonicalize_source_patch import canonicalize  # noqa: E402


class CanonicalizeSourcePatchTests(unittest.TestCase):
    def test_ignores_only_blob_ids_and_hunk_positions(self):
        older = (
            b"diff --git a/file b/file\n"
            b"index 1111111111111111111111111111111111111111..2222222222222222222222222222222222222222 100644\n"
            b"--- a/file\n+++ b/file\n"
            b"@@ -4,3 +4,3 @@ context\n before\n-old\n+new\n after\n"
        )
        moved = (
            b"diff --git a/file b/file\n"
            b"index aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa..bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb 100644\n"
            b"--- a/file\n+++ b/file\n"
            b"@@ -9,3 +9,3 @@ context\n before\n-old\n+new\n after\n"
        )

        self.assertEqual(canonicalize(older), canonicalize(moved))

    def test_rejects_changed_content_or_context(self):
        original = (
            b"diff --git a/file b/file\n--- a/file\n+++ b/file\n"
            b"@@ -1,1 +1,1 @@\n-old\n+new\n"
        )
        changed = original.replace(b"+new\n", b"+different\n")
        changed_path = original.replace(b"a/file b/file", b"a/other b/other")

        self.assertNotEqual(canonicalize(original), canonicalize(changed))
        self.assertNotEqual(canonicalize(original), canonicalize(changed_path))


if __name__ == "__main__":
    unittest.main()
