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
pub mod health;
pub mod kuadrant_executor;
pub mod kuadrant_host;
pub mod kuadrant_transport;
pub mod metrics;
pub mod response;
pub mod server;
pub mod tls;
