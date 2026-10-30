#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Set the qualification-owned GridNetwork scoring contract in Helm output.

Helm invokes this program as a post-renderer. It deliberately edits only the
single GridNetwork with both the expected name and qualification-run label.
The transform uses Python's standard library so the runner needs no YAML
package beyond Helm's already-rendered, chart-controlled document stream.
"""

from __future__ import annotations

import os
import json
import re
import sys


EXPECTED_FIELDS = {
    "routingPolicy": re.compile(r"(?m)^  routingPolicy:[ \t]*['\"]?scoreFirst['\"]?[ \t]*$"),
    "scoringPolicy": re.compile(
        r"(?m)^  scoringPolicy:[ \t]*\r?\n    strategy:[ \t]*['\"]?queueDepth['\"]?[ \t]*$"
    ),
    "selectionPolicy": re.compile(
        r"(?m)^  selectionPolicy:[ \t]*\r?\n    mode:[ \t]*['\"]?deterministic['\"]?[ \t]*$"
    ),
    "metricsRefreshInterval": re.compile(
        r"(?m)^  metricsRefreshInterval:[ \t]*['\"]?10s['\"]?[ \t]*$"
    ),
}
FIELD_NAMES = tuple(EXPECTED_FIELDS)
POLICY_BLOCK = (
    "  routingPolicy: scoreFirst\n"
    "  scoringPolicy:\n"
    "    strategy: queueDepth\n"
    "  selectionPolicy:\n"
    "    mode: deterministic\n"
    '  metricsRefreshInterval: "10s"\n'
)


def fail(message: str) -> "None":
    print(f"gridnetwork scoring post-renderer: {message}", file=sys.stderr)
    raise SystemExit(2)


def scalar_value(value: str) -> str:
    value = value.strip()
    if len(value) >= 2 and value[0] == value[-1] and value[0] in "\"'":
        return value[1:-1]
    return value


def split_stream(stream: str) -> tuple[list[str], list[str]]:
    separator = re.compile(r"(?m)^---[ \t]*\r?\n")
    return separator.split(stream), separator.findall(stream)


def rewrite_tls_secret_namespace(document: str, namespace: str) -> tuple[str, bool]:
    if len(namespace) > 63 or not re.fullmatch(r"[a-z0-9](?:[-a-z0-9]*[a-z0-9])?", namespace):
        fail("GRID_TLS_SECRET_NAMESPACE must be a Kubernetes namespace name")

    lines = document.splitlines(keepends=True)
    spec_indexes = [
        index for index, line in enumerate(lines) if re.fullmatch(r"spec:[ \t]*\r?\n?", line)
    ]
    if len(spec_indexes) != 1:
        fail("task GridNetwork must contain exactly one top-level spec")
    spec_index = spec_indexes[0]
    tls_indexes = [
        index
        for index, line in enumerate(lines[spec_index + 1 :], start=spec_index + 1)
        if re.fullmatch(r"  tls:[ \t]*\r?\n?", line)
    ]
    if len(tls_indexes) != 1:
        fail("task GridNetwork TLS references are missing or ambiguous")
    tls_index = tls_indexes[0]
    tls_end = next(
        (
            index
            for index in range(tls_index + 1, len(lines))
            if re.match(r"^  [A-Za-z][A-Za-z0-9]*:[ \t]*", lines[index])
        ),
        len(lines),
    )

    changed = False
    for reference in ("siteSecretRef", "caSecretRef"):
        reference_indexes = [
            index
            for index in range(tls_index + 1, tls_end)
            if re.fullmatch(rf"    {reference}:[ \t]*\r?\n?", lines[index])
        ]
        if len(reference_indexes) != 1:
            fail(f"task GridNetwork must contain exactly one tls.{reference}")
        reference_index = reference_indexes[0]
        reference_end = next(
            (
                index
                for index in range(reference_index + 1, tls_end)
                if re.match(r"^    [A-Za-z][A-Za-z0-9]*:[ \t]*", lines[index])
            ),
            tls_end,
        )
        name_indexes = [
            index
            for index in range(reference_index + 1, reference_end)
            if re.match(r"^      name:[ \t]*", lines[index])
        ]
        namespace_indexes = [
            index
            for index in range(reference_index + 1, reference_end)
            if re.match(r"^      namespace:[ \t]*", lines[index])
        ]
        if len(name_indexes) != 1 or len(namespace_indexes) != 1:
            fail(f"task GridNetwork tls.{reference} must contain one name and namespace")
        namespace_index = namespace_indexes[0]
        current_line = lines[namespace_index]
        newline = "\r\n" if current_line.endswith("\r\n") else "\n" if current_line.endswith("\n") else ""
        replacement = f'      namespace: "{namespace}"{newline}'
        if current_line != replacement:
            lines[namespace_index] = replacement
            changed = True
    return "".join(lines), changed


def transform_document(document: str, expected_name: str) -> tuple[str, bool, bool]:
    if not re.search(r"(?m)^kind:[ \t]*GridNetwork[ \t]*$", document):
        return document, False, False

    spec_match = re.search(r"(?m)^spec:[ \t]*\r?$", document)
    metadata_match = re.search(r"(?m)^metadata:[ \t]*\r?$", document)
    if spec_match is None or metadata_match is None or metadata_match.start() >= spec_match.start():
        fail("GridNetwork document is missing its top-level metadata or spec")

    metadata = document[metadata_match.end() : spec_match.start()]
    names = re.findall(r"(?m)^  name:[ \t]*(.*?)\r?$", metadata)
    if len(names) != 1:
        fail("GridNetwork metadata must contain exactly one name")
    if scalar_value(names[0]) != expected_name:
        return document, True, False

    run_label = os.environ["GRID_RUN_LABEL"]
    run_label_pattern = re.compile(
        rf"(?m)^    praxis\.fast/qualification-run:[ \t]*['\"]?{re.escape(run_label)}['\"]?[ \t]*$"
    )
    if run_label_pattern.search(metadata) is None:
        fail(f"GridNetwork/{expected_name} is not labeled for the expected qualification run")

    spec_body = document[spec_match.end() :]
    present = {
        field: len(re.findall(rf"(?m)^  {re.escape(field)}:[ \t]*", spec_body))
        for field in FIELD_NAMES
    }
    policy_changed = False
    if all(count == 0 for count in present.values()):
        insertion = spec_match.end()
        if document[insertion : insertion + 1] not in ("\n", "\r"):
            fail("GridNetwork spec header is malformed")
        newline_end = document.find("\n", insertion)
        if newline_end < 0:
            fail("GridNetwork spec has no body")
        insertion = newline_end + 1
        document = document[:insertion] + POLICY_BLOCK + document[insertion:]
        policy_changed = True
    elif any(count != 1 for count in present.values()):
        fail("GridNetwork has a partial or duplicate scoring policy; refusing an ambiguous transform")
    elif any(pattern.search(spec_body) is None for pattern in EXPECTED_FIELDS.values()):
        fail("GridNetwork scoring fields conflict with the qualification policy")

    tls_namespace = os.environ.get("GRID_TLS_SECRET_NAMESPACE", "").strip()
    if tls_namespace:
        document, tls_changed = rewrite_tls_secret_namespace(document, tls_namespace)
        policy_changed = policy_changed or tls_changed

    hops_json = os.environ.get("GRID_PROVIDER_HOP_ENDPOINTS_JSON", "").strip()
    if not hops_json:
        return document, True, policy_changed
    hops = validate_provider_hops(hops_json)
    gateways_match = re.search(r"(?m)^  consumerGateways:[ \t]*\r?$", document)
    if gateways_match is None:
        fail("provider-hop allowlist requested but GridNetwork has no consumerGateways field")
    following_spec_field = re.search(r"(?m)^  [A-Za-z][A-Za-z0-9]*:[ \t]*", document[gateways_match.end() :])
    block_end = gateways_match.end() + following_spec_field.start() if following_spec_field else len(document)
    gateway_block = document[gateways_match.end() : block_end]
    if len(re.findall(r"(?m)^    - name:[ \t]*", gateway_block)) != 1:
        fail("provider-hop allowlist requires exactly one rendered consumer Gateway")
    existing = re.search(r"(?m)^      providerHopEndpoints:[ \t]*(.+?)[ \t]*$", gateway_block)
    expected_block = "      providerHopEndpoints: " + json.dumps(hops, separators=(",", ":")) + "\n"
    if existing:
        try:
            if json.loads(existing.group(1)) != hops:
                fail("rendered providerHopEndpoints conflict with the verified Praxis mTLS endpoint inventory")
        except json.JSONDecodeError as error:
            raise SystemExit("gridnetwork scoring post-renderer: providerHopEndpoints are not valid JSON") from error
        return document, True, policy_changed
    prefix = document[:block_end]
    if prefix and not prefix.endswith("\n"):
        prefix += "\n"
    document = prefix + expected_block + document[block_end:]
    return document, True, True


def validate_provider_hops(raw: str) -> list[dict[str, object]]:
    try:
        hops = json.loads(raw)
        endpoints = json.loads(os.environ.get("GRID_PRAXIS_CLUSTER_ENDPOINTS_JSON", "[]"))
    except json.JSONDecodeError as error:
        raise SystemExit("gridnetwork scoring post-renderer: provider-hop endpoint input is not valid JSON") from error
    if not isinstance(hops, list) or len(hops) != 2 or not isinstance(endpoints, list) or len(endpoints) != 2:
        fail("score-based provider hops require exactly two endpoint and allowlist entries")
    if any(
        not isinstance(item, dict)
        or not isinstance(item.get("cluster"), str)
        or not item["cluster"]
        or set(item) != {"cluster", "transport"}
        or not isinstance(item.get("transport"), dict)
        or item["transport"].get("mode") != "mutual_tls"
        or not isinstance(item["transport"].get("sni"), str)
        or not item["transport"]["sni"].strip()
        for item in hops
    ):
        fail("each providerHopEndpoints entry must declare a cluster and mutual_tls transport with SNI")
    if any(
        not isinstance(item, dict)
        or not isinstance(item.get("cluster"), str)
        or not isinstance(item.get("transport"), dict)
        or item["transport"].get("mode") != "mutual_tls"
        or not isinstance(item["transport"].get("sni"), str)
        or not item["transport"]["sni"].strip()
        for item in endpoints
    ):
        fail("provider-hop allowlist requires two verified mutual_tls Praxis cluster endpoints")
    hop_pairs = {(item["cluster"], item["transport"]["sni"]) for item in hops}
    endpoint_pairs = {(item["cluster"], item["transport"]["sni"]) for item in endpoints}
    if len(hop_pairs) != 2 or hop_pairs != endpoint_pairs:
        fail("providerHopEndpoints must exactly match the generated Praxis mutual_tls cluster names and SNI values")
    return hops


def main() -> None:
    network_name = os.environ.get("GRID_NETWORK_NAME", "")
    run_label = os.environ.get("GRID_RUN_LABEL", "")
    if not re.fullmatch(r"[a-z0-9][a-z0-9.-]{0,62}", network_name):
        fail("GRID_NETWORK_NAME is missing or invalid")
    if not re.fullmatch(r"[A-Za-z0-9_.-]{1,63}", run_label):
        fail("GRID_RUN_LABEL is missing or invalid")

    documents, separators = split_stream(sys.stdin.read())
    rewritten: list[str] = []
    name_matches = 0
    changes = 0
    for document in documents:
        changed_document, is_gridnetwork, changed = transform_document(document, network_name)
        if is_gridnetwork:
            metadata_match = re.search(r"(?m)^metadata:[ \t]*\r?$", changed_document)
            spec_match = re.search(r"(?m)^spec:[ \t]*\r?$", changed_document)
            if metadata_match is not None and spec_match is not None:
                metadata = changed_document[metadata_match.end() : spec_match.start()]
                names = re.findall(r"(?m)^  name:[ \t]*(.*?)\r?$", metadata)
                if len(names) == 1 and scalar_value(names[0]) == network_name:
                    name_matches += 1
        changes += int(changed)
        rewritten.append(changed_document)

    if name_matches != 1:
        fail(f"expected exactly one task GridNetwork/{network_name}, found {name_matches}")
    output = rewritten[0]
    for index, separator in enumerate(separators):
        output += separator + rewritten[index + 1]
    sys.stdout.write(output)
    print(
        f"gridnetwork scoring post-renderer: GridNetwork/{network_name} run={run_label} "
        f"policy=queueDepth routing=scoreFirst selection=deterministic refresh=10s "
        f"providerHops={len(json.loads(os.environ.get('GRID_PROVIDER_HOP_ENDPOINTS_JSON', '[]') or '[]'))} "
        f"tlsSecretNamespace={os.environ.get('GRID_TLS_SECRET_NAMESPACE', '').strip() or 'chart-default'} "
        f"changed={str(changes == 1).lower()}",
        file=sys.stderr,
    )


if __name__ == "__main__":
    main()
