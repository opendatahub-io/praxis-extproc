# Spike: MSRV alignment with UBI10 `rust-toolset` (#10)

Issue: [opendatahub-io/praxis-extproc#10](https://github.com/opendatahub-io/praxis-extproc/issues/10)

## Summary

**Decision: wait for UBI10 `rust-toolset` ≥ 1.96, keep `rustup` in the
Containerfile until then.**

Lowering the declared MSRV to 1.92 is technically possible for this
repository's source (compile + unit tests pass on 1.92), but it is
**not** a viable product path because:

1. Transitive dependencies declare higher floors (`sqlx` 0.9 → 1.94;
   `praxis-proxy` / `praxis-ai` → 1.96).
2. It would require coordinated MSRV changes across three repositories
   for a gap that UBI repos are already closing.
3. Public UBI10 repos today ship only `rust` / `rust-toolset` **1.92.0**;
   `rust` 1.96.0 exists in RHEL/CentOS Stream 10 but has not landed in
   the UBI10 appstream snapshot we build against.

When UBI10 ships `rust-toolset` 1.96+, switch the builder stage to
`dnf install -y rust-toolset` and delete the `rustup` bootstrap.

## Current state (Aug 2026)

| Item | Version / note |
| --- | --- |
| `praxis-extproc` `rust-version` | 1.96 (`Cargo.toml`, `rust-toolchain.toml`, `clippy.toml`) |
| `praxis-proxy` tag (dependency) | v0.5.2 → `rust-version = "1.96"` |
| `praxis-ai` rev (dependency) | `e9fb521…` → `rust-version = "1.96"` |
| `sqlx` (via `praxis-ai-apis`) | 0.9.0 → `rust-version = "1.94"` |
| UBI10 `rust-toolset` (public repos) | **1.92.0** (`dnf info rust-toolset` on `ubi10/ubi`) |
| Containerfile builder base | `ubi9/ubi` + `rustup` (docs mention `ubi10`; image lags docs) |
| Effective MSRV (honoring manifests) | **1.96** |

## Investigation

### 1. What blocks lowering MSRV to 1.92?

**Declared `rust-version` (policy), not compiler errors.**

With `cargo check --ignore-rust-version` on Rust **1.92.0**, the full
workspace (including git deps `praxis-proxy` v0.5.2 and `praxis-ai`
`e9fb521…`) **builds successfully**. `cargo test --ignore-rust-version`
passes all in-tree unit, integration, and doc tests.

Without `--ignore-rust-version`, Cargo refuses 1.92 because these
packages declare `rust-version = "1.96"`:

- `praxis-extproc`, `praxis-proxy-proto`
- `praxis-proxy-core`, `praxis-proxy-filter`, `praxis-proxy-tls`
- `praxis-ai-apis`, `praxis-ai-filters`

Additionally, `sqlx` 0.9.0 declares **1.94.0**, so even a coordinated
drop to 1.92 in Praxis repos would still require either:

- downgrading / replacing `sqlx` in `praxis-ai`, or
- ignoring dependency MSRV (not acceptable for release images).

**Language / edition:** `edition = "2024"` is supported on 1.92; no
1.96-only syntax was required for the current tree.

### 2. When will UBI10 Toolset ship ≥ 1.96?

- **Today (UBI10 public appstream):** latest `rust` / `rust-toolset` is
  **1.92.0-1.el10** (`registry.access.redhat.com/ubi10/ubi`).
- **RHEL/CentOS Stream 10:** `rust-1.96.0-1.el10` is published (June
  2026 per [rpmfind](https://www.rpmfind.net/linux/RPM/centos-stream/10/appstream/x86_64/rust-1.96.0-1.el10.x86_64.html));
  prior stream releases include 1.94.1 and 1.95.0.
- **UBI lag:** UBI appstream snapshots trail Stream; expect `rust-toolset`
  1.96 in a future UBI10 refresh (no public date committed in this spike).
- Rust Toolset is a rolling appstream on RHEL; Red Hat documents support
  for the latest shipped version only.

**Action:** re-check `dnf list rust-toolset` on `ubi10/ubi` before
dropping `rustup`; no manual toolchain pin should be needed once 1.96
appears in UBI repos.

### 3. Decide: wait, lower MSRV, or keep rustup?

| Option | Verdict |
| --- | --- |
| **Lower MSRV to 1.92** | Rejected — `sqlx` floor 1.94 + cross-repo 1.96 policy; high churn, short benefit. |
| **Lower MSRV to 1.94** | Rejected — still below UBI10 toolset gap and below Praxis 1.96 policy. |
| **Wait for UBI `rust-toolset` 1.96+** | **Recommended** — matches Praxis MSRV with no dependency churn. |
| **Keep `rustup` until then** | **Required today** — only way to build MSRV 1.96 on current UBI10. |

## Recommended follow-ups

1. **Now:** migrate Containerfile builder/runtime from `ubi9` → `ubi10`
   while keeping `rustup` (aligns with docs and FIPS work; no MSRV change).
2. **When UBI10 lists `rust-toolset` ≥ 1.96:** replace rustup block with:

   ```dockerfile
   RUN dnf install -y gcc gcc-c++ cmake make perl openssl-devel rust-toolset \
       && dnf clean all
   ENV PATH="/opt/rh/rust-toolset/root/usr/bin:${PATH}"
   ```

   (Exact `scl`/`PATH` may vary — verify in the target image after the
   RPM lands.)

3. **Optional guard:** add a periodic CI job or release checklist step:
   `podman run ubi10/ubi dnf list rust-toolset` and fail with a reminder
   when version ≥ 1.96 so we switch the Containerfile promptly.

4. **Do not** lower `rust-version` in this repo alone to unblock toolset;
   track MSRV with `praxis-proxy/praxis` and `praxis-proxy/ai`.

## Commands used (reproduce)

```console
# UBI10 toolset version
podman run --rm registry.access.redhat.com/ubi10/ubi:latest \
  dnf info rust-toolset

# Compile / test on 1.92 ignoring declared MSRV (language compatibility)
rustup install 1.92.0
cd praxis-extproc
rustup run 1.92.0 cargo check --ignore-rust-version
rustup run 1.92.0 cargo test --ignore-rust-version
```

## References

- [#10 — Align MSRV with UBI10 rust-toolset](https://github.com/opendatahub-io/praxis-extproc/issues/10)
- [Red Hat Rust Toolset container images](https://docs.redhat.com/en/documentation/red_hat_developer_tools/1/html/using_rust_1.88.0_toolset/container-images-with-rust-toolset)
- `praxis-proxy` workspace `rust-version = "1.96"` (tag v0.5.2)
