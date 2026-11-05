//! Conformance-profile tests: the production-faithful chain (`ai_guardrails` +
//! routing) and general proxy behavior. Run with `PROFILE=conformance`
//! (`cargo test --test k8s_e2e conformance`).

mod completions;
mod direct;
mod errors;
mod filters;
mod guardrails;
mod malformed_json;
mod routing;
