// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Red Hat, Inc.

#![deny(unsafe_code)]
#![deny(unreachable_pub)]

//! Envoy ExtProc server for Praxis filter pipelines.
//!
//! Translates Envoy external processing gRPC messages into Praxis
//! [`FilterPipeline`] invocations, enabling Praxis filters to run
//! alongside Envoy.
//!
//! [`FilterPipeline`]: praxis_filter::FilterPipeline

pub mod adapter;
pub mod config;
pub mod error;
pub mod fips;
pub mod grpc_limits;
mod handlers;
pub mod health;
pub mod metrics;
mod pipeline;
mod protocol;
pub mod response;
pub mod server;
pub mod tls;

/// Qualification-only helpers (`e2e_body_oracle`); compiled with `--features k8s-e2e`.
#[cfg(feature = "k8s-e2e")]
pub mod e2e;

#[cfg(test)]
mod test_support;
