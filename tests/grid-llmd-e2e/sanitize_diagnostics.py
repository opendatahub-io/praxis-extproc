#!/usr/bin/env python3
"""Redact credential-like values from captured Kubernetes diagnostics."""

from __future__ import annotations

import re
import sys


_PRIVATE_KEY = re.compile(
    r"-----BEGIN [^-]*PRIVATE KEY-----.*?-----END [^-]*PRIVATE KEY-----",
    re.IGNORECASE | re.DOTALL,
)
_AUTHORIZATION = re.compile(
    r"(?i)([\"']?authorization[\"']?[ \t]*[:=][ \t]*)(\"[^\"\r\n]*\"|'[^'\r\n]*'|(?:bearer[ \t]+)?[^\s,;}]+(?:[ \t]+[^\s,;}]+)?)"
)
_CREDENTIAL_ASSIGNMENT = re.compile(
    r"(?i)([\"']?[A-Z0-9_.-]*(?:TOKEN|PASSWORD|PASSWD|SECRET|CREDENTIAL|API[_-]?KEY|PRIVATE[_-]?KEY|CLIENT[_-]?SECRET|ACCESS[_-]?KEY)[A-Z0-9_.-]*[\"']?[ \t]*[:=][ \t]*)"
    r"(\"[^\"\r\n]*\"|'[^'\r\n]*'|[^\s,;}\]]+)"
)


def _redact_assignment(match: re.Match[str]) -> str:
    value = match.group(2)
    if value.startswith('"') and value.endswith('"'):
        replacement = '"[REDACTED]"'
    elif value.startswith("'") and value.endswith("'"):
        replacement = "'[REDACTED]'"
    else:
        replacement = "[REDACTED]"
    return match.group(1) + replacement


def sanitize(text: str) -> str:
    """Redact private-key blocks and values assigned to credential-like keys."""
    text = _PRIVATE_KEY.sub("[REDACTED PRIVATE KEY]", text)
    text = _AUTHORIZATION.sub(_redact_assignment, text)
    return _CREDENTIAL_ASSIGNMENT.sub(_redact_assignment, text)


if __name__ == "__main__":
    sys.stdout.write(sanitize(sys.stdin.read()))
