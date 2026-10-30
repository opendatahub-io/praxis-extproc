#!/usr/bin/env python3
"""Redact sensitive values before existing-platform snapshots enter evidence."""

from __future__ import annotations

import json
import re
import sys


SENSITIVE = {
    "authorization",
    "password",
    "passwd",
    "token",
    "apikey",
    "credential",
    "credentialvalue",
    "privatekey",
    "tlskey",
    "clientsecret",
    "secretdata",
    "connectionurl",
    "databaseurl",
    "dbconnectionurl",
}


def normalize(key: str) -> str:
    return "".join(char.lower() for char in key if char.isalnum())


def redact(value):
    if isinstance(value, dict):
        result = {}
        for key, child in value.items():
            normalized = normalize(key)
            is_reference = normalized.endswith("ref") or normalized.endswith("secretname")
            sensitive = normalized in SENSITIVE or any(
                normalized.endswith(suffix) for suffix in ("password", "apikey", "token", "privatekey", "clientsecret")
            )
            result[key] = "[REDACTED]" if sensitive and not is_reference else redact(child)
        return result
    if isinstance(value, list):
        return [redact(item) for item in value]
    if isinstance(value, str):
        return re.sub(r"(?i)(https?://)[^/@\s:]+:[^/@\s]+@", r"\1[REDACTED]@", value)
    return value


def main() -> int:
    document = json.load(sys.stdin)
    json.dump(redact(document), sys.stdout, indent=2, sort_keys=True)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
