//! Integrity-profile tests: the instrumented chain (`identity_header_guard`
//! plus the body and header oracles, no response guardrails) and the body/header
//! mechanics it validates. Run with `PROFILE=integrity`
//! (`cargo test --test k8s_e2e integrity`).

mod chained_oracle;
mod fd_streamed_oracle;
mod header_protection;
mod streaming;
