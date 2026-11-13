#!/usr/bin/env python3
"""Redact credential-like values from captured Kubernetes diagnostics."""

from __future__ import annotations

import json
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
    r"(\"[^\"\r\n]*\"|'[^'\r\n]*'|[^\s,;{}\[\]]+)"
)
_CAMEL_BOUNDARY = re.compile(r"([a-z0-9])([A-Z])")


def _normalized_key(key: str) -> str:
    key = _CAMEL_BOUNDARY.sub(r"\1_\2", key)
    return re.sub(r"[^a-zA-Z0-9]+", "_", key).strip("_").lower()


def _is_sensitive_key(key: str) -> bool:
    normalized = _normalized_key(key)
    if normalized.endswith(("_ref", "_name", "_namespace")) and any(
        part in normalized for part in ("secret", "credential")
    ):
        return False
    if normalized.startswith(("enable_", "disable_", "supports_", "allow_")) and normalized.endswith(
        ("_credential", "_credentials")
    ):
        return False
    sensitive_names = {
        "authorization",
        "password",
        "passwd",
        "secret",
        "credential",
        "credentials",
        "token",
    }
    return normalized in sensitive_names or normalized.endswith(
        (
            "_password",
            "_passwd",
            "_secret",
            "_credential",
            "_credentials",
            "_token",
            "_api_key",
            "_client_key",
            "_private_key",
            "_access_key",
        )
    )


def _redact_tree(value: object) -> object:
    if isinstance(value, dict):
        return {key: _redact_tree(child) for key, child in value.items()}
    if isinstance(value, list):
        return [_redact_tree(child) for child in value]
    return "[REDACTED]"


def _sanitize_json_value(value: object, *, in_secret: bool = False) -> object:
    if isinstance(value, dict):
        is_secret = in_secret or value.get("kind") == "Secret"
        sanitized: dict[str, object] = {}
        for key, child in value.items():
            if is_secret and key in {"data", "stringData"}:
                sanitized[key] = _redact_tree(child)
            elif _is_sensitive_key(key):
                sanitized[key] = "[REDACTED]"
            else:
                sanitized[key] = _sanitize_json_value(child, in_secret=is_secret)
        return sanitized
    if isinstance(value, list):
        return [_sanitize_json_value(child, in_secret=in_secret) for child in value]
    if isinstance(value, str):
        return _sanitize_text(value)
    return value


def _redact_assignment(match: re.Match[str]) -> str:
    key = match.group(1).split(":", maxsplit=1)[0].split("=", maxsplit=1)[0]
    key = key.strip().strip("\"'")
    if not _is_sensitive_key(key):
        return match.group(0)

    value = match.group(2)
    if value.startswith('"') and value.endswith('"'):
        replacement = '"[REDACTED]"'
    elif value.startswith("'") and value.endswith("'"):
        replacement = "'[REDACTED]'"
    else:
        replacement = "[REDACTED]"
    return match.group(1) + replacement


def sanitize(text: str) -> str:
    """Redact credentials while preserving valid structured diagnostic data."""
    text = _PRIVATE_KEY.sub("[REDACTED PRIVATE KEY]", text)
    try:
        parsed = json.loads(text)
    except json.JSONDecodeError:
        pass
    else:
        return json.dumps(_sanitize_json_value(parsed), indent=2, ensure_ascii=False) + "\n"
    return _sanitize_text(text)


def _sanitize_text(text: str) -> str:
    """Redact credential scalars in non-JSON text and embedded strings."""
    text = _PRIVATE_KEY.sub("[REDACTED PRIVATE KEY]", text)
    text = _AUTHORIZATION.sub(_redact_assignment, text)
    return _CREDENTIAL_ASSIGNMENT.sub(_redact_assignment, text)


if __name__ == "__main__":
    sys.stdout.write(sanitize(sys.stdin.read()))
