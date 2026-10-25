//! Extended (qualification) k8s e2e tier — `#[ignore]` by default.
//!
//! Run: `make test-e2e-extended` or
//! `cargo test --features k8s-e2e --test k8s_e2e -- extended --ignored`

mod chained;
mod fd_streamed;
mod helpers;
mod idle_tls;
mod report;

pub(crate) use report::load_profile_name;
