#!/usr/bin/env bash
# Verify that the boundary contracts kill representative production mutations.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
logs="$root/target/boundary-mutations"
mkdir -p "$logs"
cp -a "$root"/{Cargo.toml,Cargo.lock,rust-toolchain.toml,rustfmt.toml,clippy.toml,src,proto,xtask,tests,examples,.cargo} "$scratch/"

run_contract() {
  cargo test --manifest-path "$scratch/Cargo.toml" --target-dir "$root/target" \
    --test grpc "$1" -- --exact > "$logs/$2.log" 2>&1
  grep -q '^test result: ok. 1 passed;' "$logs/$2.log"
}

expect_failure() {
  if run_contract "$1" "$2"; then
    echo "Mutation survived: $2 (see $logs/$2.log)" >&2
    exit 1
  fi
  if ! grep -q '^test result: FAILED' "$logs/$2.log"; then
    echo "Mutation did not reach a test assertion: $2 (see $logs/$2.log)" >&2
    exit 1
  fi
  echo "Detected: $2"
}

boundary=adapter::payloads::body_cap_boundaries_preserve_content_and_recover_on_the_same_channel
payload=adapter::payloads::exact_size_multibyte_payload_reaches_body_filter_without_truncation
run_contract "$boundary" baseline-boundary
run_contract "$payload" baseline-payload

sed -i 's/current.saturating_add(incoming) > max/current.saturating_add(incoming) >= max/' "$scratch/src/handlers.rs"
expect_failure "$boundary" reject-exact-cap
cp "$root/src/handlers.rs" "$scratch/src/handlers.rs"

sed -i 's/current.saturating_add(incoming) > max/current.saturating_add(incoming) > max.saturating_add(1)/' "$scratch/src/handlers.rs"
expect_failure "$boundary" accept-one-byte-over-cap
cp "$root/src/handlers.rs" "$scratch/src/handlers.rs"

sed -i 's/body: chunk.to_vec(),/body: chunk.get(..chunk.len().saturating_sub(1)).unwrap_or_default().to_vec(),/' "$scratch/src/response.rs"
expect_failure "$payload" truncate-released-chunk

echo "All boundary mutations detected; logs: $logs"
