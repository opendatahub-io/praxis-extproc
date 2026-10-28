#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Timestamped, append-only evidence primitives for ExtProc cross-component E2E.

set -Eeuo pipefail

evidence_init() {
  local evidence_dir="$1" run_id="$2" scenario="$3" root_dir="$4" captured_at
  [[ "$scenario" == grid-llmd || "$scenario" == grid-maas ]] || return 2
  [[ "$run_id" =~ ^[A-Za-z0-9]([A-Za-z0-9._-]{0,61}[A-Za-z0-9])?$ ]] || return 2
  [[ ! -e "$evidence_dir" ]] || { printf 'evidence directory already exists\n' >&2; return 2; }
  umask 077
  mkdir -p "$evidence_dir"
  chmod 0700 "$evidence_dir"
  captured_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  cat >"$evidence_dir/RUN-METADATA.json" <<EOF
{"schema":"praxis-extproc.e2e-run/v1","run_id":"$run_id","scenario":"$scenario","started_at":"$captured_at"}
EOF
  chmod 0600 "$evidence_dir/RUN-METADATA.json"
  printf '# Run log\n\n' >"$evidence_dir/RUN-LOG.md"
  printf '# Status\n\n- Run: `%s`\n- Scenario: `%s`\n- Started: `%s`\n' "$run_id" "$scenario" "$captured_at" >"$evidence_dir/STATUS.md"
  printf '# Decisions\n\n' >"$evidence_dir/DECISIONS.md"
  printf '# Bug capture\n\n' >"$evidence_dir/BUG-CAPTURE.md"
  printf '# Next step\n\n' >"$evidence_dir/NEXT-STEP.md"
  printf '# Final result\n\n- Status: NOT RUN\n' >"$evidence_dir/FINAL-RESULT.md"
  mkdir -p "$root_dir"
  for register in RUN-LOG.md STATUS.md DECISIONS.md BUG-CAPTURE.md NEXT-STEP.md FINAL-RESULT.md; do
    if [[ ! -e "$root_dir/$register" ]]; then
      printf '# %s\n\n' "${register%.md}" >"$root_dir/$register"
    fi
  done
}

evidence_record_phase() {
  local evidence_dir="$1" root_dir="$2" phase="$3" started_at="$4" hypothesis="$5" command_record="$6"
  local expected="$7" actual="$8" status="$9" evidence_path="${10}" retain_rollback="${11}" next_step="${12}"
  local captured_at sanitized_command
  captured_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  sanitized_command="$(printf '%s' "$command_record" | sed -E 's/(--(token|password|client-secret|authorization)(=|[[:space:]])?)[^[:space:]]+/\1[REDACTED]/Ig')"
  {
    printf '\n## %s — %s\n\n' "$captured_at" "$phase"
    printf -- '- Started: `%s`\n- Hypothesis: %s\n- Command/change: `%s`\n' "$started_at" "$hypothesis" "$sanitized_command"
    printf -- '- Expected: %s\n- Actual: %s\n- Result: `%s`\n' "$expected" "$actual" "$status"
    printf -- '- Evidence: `%s`\n- Retain/rollback: %s\n- Next executable step: %s\n' "$evidence_path" "$retain_rollback" "$next_step"
  } | tee -a "$evidence_dir/RUN-LOG.md" "$root_dir/RUN-LOG.md" >/dev/null
  {
    printf '\n- `%s` — **%s**: %s; evidence `%s`.\n' "$captured_at" "$status" "$phase" "$evidence_path"
  } | tee -a "$evidence_dir/STATUS.md" "$root_dir/STATUS.md" >/dev/null
  if [[ "$status" == FAIL || "$status" == BLOCKED ]]; then
    {
      printf '\n## %s — %s\n\n- Hypothesis: %s\n- Actual: %s\n- Evidence: `%s`\n- Likely owner: %s\n- Reproduction: %s\n- Proposed fix: %s\n- Acceptance criteria: %s\n' \
        "$captured_at" "$phase" "$hypothesis" "$actual" "$evidence_path" "record in phase report" "$sanitized_command" "$next_step" "$expected"
    } | tee -a "$evidence_dir/BUG-CAPTURE.md" "$root_dir/BUG-CAPTURE.md" >/dev/null
  fi
  {
    printf '\n## %s — %s\n\n- Decision: %s\n- Evidence: `%s`\n' "$captured_at" "$phase" "$retain_rollback" "$evidence_path"
  } | tee -a "$evidence_dir/DECISIONS.md" "$root_dir/DECISIONS.md" >/dev/null
  {
    printf '\n## %s\n\n%s\n' "$captured_at" "$next_step"
  } | tee -a "$evidence_dir/NEXT-STEP.md" "$root_dir/NEXT-STEP.md" >/dev/null
}

evidence_finalize() {
  local evidence_dir="$1" root_dir="$2" status="$3" result="$4" next_step="$5" captured_at
  [[ "$status" == PASS || "$status" == PARTIAL || "$status" == BLOCKED || "$status" == NOT\ RUN ]] || return 2
  captured_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  {
    printf '\n## %s\n\n- Status: **%s**\n- Result: %s\n- Next step: %s\n' "$captured_at" "$status" "$result" "$next_step"
  } | tee -a "$evidence_dir/FINAL-RESULT.md" "$root_dir/FINAL-RESULT.md" >/dev/null
  {
    printf '\n- Final status at `%s`: **%s** — %s\n' "$captured_at" "$status" "$result"
  } | tee -a "$evidence_dir/STATUS.md" "$root_dir/STATUS.md" >/dev/null
}
