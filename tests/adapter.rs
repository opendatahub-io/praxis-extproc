// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Red Hat, Inc.

//! Adapter / transport mechanics: the wire <-> `HttpFilterContext` <->
//! `ProcessingResponse` path, plus gRPC framing limits, exercised with stub
//! filters so the server machinery is tested independently of real filters.
//!
//! Starts a tonic server on a random port, sends `ProcessingRequest`
//! messages via a tonic client, and verifies responses.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::tests_outside_test_module,
    clippy::too_many_lines,
    clippy::cognitive_complexity,
    clippy::missing_assert_message,
    clippy::needless_raw_strings,
    clippy::needless_raw_string_hashes,
    clippy::missing_docs_in_private_items,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::future_not_send,
    clippy::large_futures,
    clippy::needless_pass_by_value,
    reason = "tests"
)]
#![allow(missing_docs, reason = "test module")]


#[path = "adapter/support.rs"]
mod support;

#[path = "adapter/basics.rs"]
mod basics;

#[path = "adapter/body_modes.rs"]
mod body_modes;

#[path = "adapter/sequencing.rs"]
mod sequencing;

#[path = "adapter/limits.rs"]
mod limits;

#[path = "adapter/validation.rs"]
mod validation;

#[path = "adapter/routing.rs"]
mod routing;
