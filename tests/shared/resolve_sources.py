#!/usr/bin/env python3
"""Resolve integration sources into clean detached worktrees and evidence."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shlex
import subprocess
import sys
import tempfile
from datetime import datetime, timezone
from pathlib import Path
from urllib.parse import urlsplit, urlunsplit


ROOT = Path(__file__).resolve().parents[2]
LOCK_FILE = Path(__file__).with_name("locked-source-refs.json")
SHA_RE = re.compile(r"^[0-9a-f]{40}$")
SOURCE_DEFAULTS = {
    "aigc": "main",
    "maas": "main",
    "extproc": "main",
    "praxis-ai": "main",
    "grid": "main",
    "grid-operator": "main",
    "gitops": "main",
    "kuadrant": "v1.4.2",
    "epp": "main",
    "llm-d-sim": "main",
}
REF_ENV = {
    "aigc": "AIGC_REF",
    "maas": "MAAS_REF",
    "extproc": "EXTPROC_REF",
    "praxis-ai": "PRAXIS_AI_REF",
    "grid": "GRID_REF",
    "grid-operator": "GRID_OPERATOR_REF",
    "gitops": "GITOPS_REF",
    "kuadrant": "KUADRANT_REF",
    "epp": "EPP_REF",
    "llm-d-sim": "LLM_D_SIM_REF",
}
REPOSITORY_ENV = {
    "aigc": "AIGC_REPOSITORY_URL",
    "maas": "MAAS_REPOSITORY_URL",
    "extproc": "EXTPROC_REPOSITORY_URL",
    "praxis-ai": "PRAXIS_AI_REPOSITORY_URL",
    "grid": "GRID_REPOSITORY_URL",
    "grid-operator": "GRID_OPERATOR_REPOSITORY_URL",
    "gitops": "GITOPS_REPOSITORY_URL",
    "kuadrant": "KUADRANT_REPOSITORY_URL",
    "epp": "EPP_REPOSITORY_URL",
    "llm-d-sim": "LLM_D_SIM_REPOSITORY_URL",
}
CALLER_DIR_ENV = {
    "aigc": "AIGC_SOURCE_DIR",
    "maas": "MAAS_SOURCE_DIR",
    "extproc": "EXTPROC_SOURCE_DIR",
    "praxis-ai": "PRAXIS_AI_SOURCE_DIR",
    "grid": "GRID_SOURCE_DIR",
    "grid-operator": "GRID_OPERATOR_SOURCE_DIR",
    "gitops": "GITOPS_SOURCE_DIR",
    "kuadrant": "KUADRANT_SOURCE_DIR",
    "epp": "EPP_SOURCE_DIR",
    "llm-d-sim": "LLM_D_SIM_SOURCE_DIR",
}


class ResolutionError(RuntimeError):
    """Raised for unsafe or incomplete source resolution."""


def run_git(args: list[str], *, cwd: Path | None = None, binary: bool = False) -> bytes | str:
    env = os.environ.copy()
    env["GIT_TERMINAL_PROMPT"] = "0"
    env["GIT_OPTIONAL_LOCKS"] = "0"
    try:
        result = subprocess.run(
            ["git", *args],
            cwd=cwd,
            env=env,
            check=False,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
    except OSError as error:
        raise ResolutionError(f"could not execute git: {error.strerror}") from error
    if result.returncode != 0:
        raise ResolutionError(f"git operation failed (exit {result.returncode})")
    return result.stdout if binary else result.stdout.decode("utf-8", errors="strict").strip()


def parse_pairs(values: list[str], option: str) -> dict[str, str]:
    parsed: dict[str, str] = {}
    for value in values:
        component, separator, item = value.partition("=")
        if not separator or not component or not item or component in parsed:
            raise ResolutionError(f"{option} entries must be unique component=value pairs")
        parsed[component] = item
    return parsed


def sanitize_remote(remote: str) -> str:
    if "@" in remote and ":" in remote and not remote.startswith(("https://", "http://", "ssh://", "file://")):
        user, host_path = remote.split("@", 1)
        if user and "/" not in user:
            host, separator, path = host_path.partition(":")
            if not separator or not host or not path:
                raise ResolutionError("unsupported SSH source remote")
            return f"ssh://{host}/{path.lstrip('/')}"
    parsed = urlsplit(remote)
    if parsed.scheme and parsed.netloc:
        if parsed.username or parsed.password:
            raise ResolutionError("source remote URL must not contain embedded credentials")
        return urlunsplit((parsed.scheme, parsed.hostname or "", parsed.path, "", ""))
    if parsed.scheme == "file":
        return urlunsplit(("file", "", parsed.path, "", ""))
    if remote.startswith(("./", "../", "/")):
        return remote
    raise ResolutionError("source remote must be a credential-free URL or local path")


def hash_caller_tree(source_dir: Path | None) -> tuple[str, str, str, str, str]:
    empty_hash = hashlib.sha256(b"").hexdigest()
    if source_dir is None:
        return "not-provided", empty_hash, empty_hash, "not-provided", "not-provided"
    if not source_dir.is_dir():
        raise ResolutionError(f"caller source directory does not exist: {source_dir}")
    try:
        diff = run_git(["diff", "HEAD", "--binary"], cwd=source_dir, binary=True)
        untracked = run_git(
            ["ls-files", "--others", "--exclude-standard", "-z"], cwd=source_dir, binary=True
        )
    except ResolutionError:
        return "not-a-git-worktree", empty_hash, empty_hash, "unknown", "unknown"
    assert isinstance(diff, bytes) and isinstance(untracked, bytes)
    digest = hashlib.sha256()
    for raw_path in sorted(filter(None, untracked.split(b"\0"))):
        path = source_dir / os.fsdecode(raw_path)
        digest.update(len(raw_path).to_bytes(8, "big"))
        digest.update(raw_path)
        if path.is_symlink():
            content = os.fsencode(os.readlink(path))
        elif path.is_file():
            content = path.read_bytes()
        elif path.is_dir():
            content = b"directory"
        else:
            content = b"missing"
        digest.update(hashlib.sha256(content).digest())
    state = run_git(["status", "--porcelain", "--untracked-files=all"], cwd=source_dir)
    branch = run_git(["branch", "--show-current"], cwd=source_dir)
    head = run_git(["rev-parse", "HEAD"], cwd=source_dir)
    assert isinstance(state, str)
    assert isinstance(branch, str) and isinstance(head, str)
    return (
        "dirty" if state else "clean",
        hashlib.sha256(diff).hexdigest(),
        digest.hexdigest(),
        branch or "detached",
        head,
    )


def selected_components(scenario: str, platform_manifests: bool) -> list[str]:
    if scenario == "grid-maas":
        components = ["aigc", "maas", "extproc", "praxis-ai", "grid", "grid-operator", "kuadrant", "epp", "llm-d-sim"]
    else:
        # MaaS remains a required RHOAI platform prerequisite for the native
        # Grid lane, even though the AIGC/ExternalModel front door is optional.
        components = ["maas", "extproc", "praxis-ai", "grid", "grid-operator", "epp", "llm-d-sim"]
    if platform_manifests:
        components.append("gitops")
    return components


def source_ref(component: str, mode: str, explicit: str | None, locked: dict) -> tuple[str, str | None]:
    lock_entry = locked["components"].get(component)
    if lock_entry is None:
        raise ResolutionError(f"locked source is missing for {component}")
    locked_ref = lock_entry["ref"]
    default = SOURCE_DEFAULTS[component] if mode == "main" else locked_ref
    requested = explicit if explicit is not None else default
    if mode == "locked" and explicit is not None and not SHA_RE.fullmatch(explicit):
        if component != "kuadrant" or explicit != lock_entry.get("release"):
            raise ResolutionError(f"locked mode requires an immutable full SHA override for {component}")
    if mode == "locked" and component == "kuadrant" and explicit is None:
        requested = locked_ref
    # `main` is an explicit moving source channel: resolve the branch to a SHA
    # and lock that SHA in the run evidence. The checked-in lock is enforced
    # only when callers select `locked`.
    expected = locked_ref if mode == "locked" else None
    return requested, expected


def resolve_component(
    component: str,
    remote: str,
    requested_ref: str,
    expected_sha: str | None,
    caller_dir: Path | None,
    work_root: Path,
) -> dict:
    sanitized_remote = sanitize_remote(remote)
    repo_dir = work_root / component / "repo"
    worktree = work_root / component / "worktree"
    repo_dir.parent.mkdir(parents=True, exist_ok=True)
    clone_env = os.environ.copy()
    clone_env["GIT_TERMINAL_PROMPT"] = "0"
    clone_env["GIT_OPTIONAL_LOCKS"] = "0"
    clone = subprocess.run(
        ["git", "clone", "--no-checkout", remote, str(repo_dir)],
        env=clone_env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    if clone.returncode != 0:
        raise ResolutionError(f"could not clone source for {component} (exit {clone.returncode})")
    run_git(["fetch", "--no-tags", "origin", requested_ref], cwd=repo_dir)
    resolved_sha = run_git(["rev-parse", "FETCH_HEAD^{commit}"], cwd=repo_dir)
    assert isinstance(resolved_sha, str)
    if not SHA_RE.fullmatch(resolved_sha):
        raise ResolutionError(f"resolved revision for {component} is not a full commit SHA")
    if expected_sha and resolved_sha != expected_sha:
        raise ResolutionError(f"locked revision mismatch for {component}")
    run_git(["worktree", "add", "--detach", str(worktree), resolved_sha], cwd=repo_dir)
    dirty_state = run_git(["status", "--porcelain", "--untracked-files=all"], cwd=worktree)
    assert isinstance(dirty_state, str)
    if dirty_state:
        raise ResolutionError(f"resolved worktree for {component} is not clean")
    archive = run_git(["archive", "--format=tar", resolved_sha], cwd=repo_dir, binary=True)
    assert isinstance(archive, bytes)
    caller_state, tracked_hash, untracked_hash, caller_branch, caller_head = hash_caller_tree(caller_dir)
    return {
        "remote_url": sanitized_remote,
        "requested_ref": requested_ref,
        "resolved_sha": resolved_sha,
        "branch_state": "detached",
        "resolved_worktree_clean": True,
        "caller_worktree_state": caller_state,
        "caller_branch_state": caller_branch,
        "caller_head": caller_head,
        "caller_tracked_diff_sha256": tracked_hash,
        "caller_untracked_content_sha256": untracked_hash,
        "build_context_sha256": hashlib.sha256(archive).hexdigest(),
        "worktree_path": str(worktree),
    }


def write_outputs(evidence_dir: Path, work_root: Path, run_record: dict) -> None:
    evidence_dir.mkdir(parents=True, exist_ok=True)
    json_path = evidence_dir / "source-locks.json"
    markdown_path = evidence_dir / "source-locks.md"
    if json_path.exists() or markdown_path.exists():
        raise ResolutionError("source-lock evidence already exists; use a new run evidence directory")
    json_path.write_text(json.dumps(run_record, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    lines = [
        "# Source locks",
        "",
        f"- Captured at: `{run_record['captured_at']}`",
        f"- Scenario: `{run_record['scenario']}`",
        f"- SOURCE_MODE: `{run_record['source_mode']}`",
        f"- Platform manifests: `{str(run_record['platform_manifests']).lower()}`",
        "",
        "| Component | Remote | Requested ref | Resolved SHA | Resolved branch | Caller branch / HEAD | Caller state | Tracked diff | Untracked content | Build context |",
        "| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |",
    ]
    for component, record in run_record["components"].items():
        lines.append(
            f"| `{component}` | `{record['remote_url']}` | `{record['requested_ref']}` | "
            f"`{record['resolved_sha']}` | `{record['branch_state']}` | "
            f"`{record['caller_branch_state']}` / `{record['caller_head']}` | `{record['caller_worktree_state']}` | "
            f"`{record['caller_tracked_diff_sha256']}` | `{record['caller_untracked_content_sha256']}` | "
            f"`{record['build_context_sha256']}` |"
        )
    markdown_path.write_text("\n".join(lines) + "\n", encoding="utf-8")
    env_lines = [f"SOURCE_WORKTREE_ROOT={shlex.quote(str(work_root))}"]
    for component, record in run_record["components"].items():
        key = re.sub(r"[^A-Za-z0-9]", "_", component).upper()
        env_lines.append(f"SOURCE_{key}_WORKTREE={shlex.quote(record['worktree_path'])}")
        env_lines.append(f"SOURCE_{key}_SHA={shlex.quote(record['resolved_sha'])}")
    env_path = work_root / "resolved-sources.env"
    env_path.write_text("\n".join(env_lines) + "\n", encoding="utf-8")
    os.chmod(json_path, 0o600)
    os.chmod(markdown_path, 0o600)
    os.chmod(env_path, 0o600)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--scenario", choices=("grid-llmd", "grid-maas"), default=os.getenv("E2E_SCENARIO", "grid-llmd"))
    parser.add_argument("--source-mode", choices=("main", "locked"), default=os.getenv("SOURCE_MODE", "main"))
    parser.add_argument("--evidence-dir", type=Path, required=True)
    parser.add_argument("--worktree-root", type=Path)
    parser.add_argument("--caller-repo-root", type=Path, default=ROOT)
    parser.add_argument("--lock-file", type=Path, default=LOCK_FILE)
    parser.add_argument("--platform-manifests", choices=("true", "false"), default=os.getenv("E2E_USE_PLATFORM_MANIFESTS", "true"))
    parser.add_argument("--repository", action="append", default=[], metavar="COMPONENT=URL")
    parser.add_argument("--caller-source-dir", action="append", default=[], metavar="COMPONENT=PATH")
    args = parser.parse_args()
    try:
        locked = json.loads(args.lock_file.read_text(encoding="utf-8"))
        if locked.get("schema") != "praxis-extproc.source-locks/v1":
            raise ResolutionError("unsupported locked source metadata schema")
        overrides = parse_pairs(args.repository, "--repository")
        caller_overrides = parse_pairs(args.caller_source_dir, "--caller-source-dir")
        selected = selected_components(args.scenario, args.platform_manifests == "true")
        work_root = args.worktree_root or Path(tempfile.mkdtemp(prefix="praxis-e2e-sources-"))
        work_root.mkdir(parents=True, exist_ok=True)
        if any(work_root.iterdir()):
            raise ResolutionError("source worktree root must be empty")
        components: dict[str, dict] = {}
        for component in selected:
            lock_entry = locked["components"].get(component)
            if lock_entry is None:
                raise ResolutionError(f"no locked source metadata for {component}")
            requested, expected = source_ref(component, args.source_mode, os.getenv(REF_ENV[component]), locked)
            repository = overrides.get(component, os.getenv(REPOSITORY_ENV[component], lock_entry["repository"]))
            caller_value = caller_overrides.get(component, os.getenv(CALLER_DIR_ENV[component]))
            if component == "extproc" and caller_value is None:
                caller_dir = args.caller_repo_root
            else:
                caller_dir = Path(caller_value) if caller_value else None
            components[component] = resolve_component(
                component,
                repository,
                requested,
                expected,
                caller_dir,
                work_root,
            )
        run_record = {
            "schema": "praxis-extproc.source-locks-run/v1",
            "captured_at": datetime.now(timezone.utc).isoformat(timespec="seconds").replace("+00:00", "Z"),
            "scenario": args.scenario,
            "source_mode": args.source_mode,
            "platform_manifests": args.platform_manifests == "true",
            "worktree_root": str(work_root),
            "components": components,
        }
        write_outputs(args.evidence_dir, work_root, run_record)
        print(f"source locks recorded: {args.evidence_dir / 'source-locks.json'}")
        return 0
    except (OSError, json.JSONDecodeError, ResolutionError, KeyError, ValueError) as error:
        print(f"source resolution failed: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
