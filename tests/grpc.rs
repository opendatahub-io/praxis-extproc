// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Red Hat, Inc.

//! Tier 1: the ext-proc server driven directly over gRPC.
//!
//! The test is the `ext_proc` client: it starts an in-process ext-proc server on
//! a random port, sends `ProcessingRequest` messages over real HTTP/2 (tonic),
//! and asserts on the `ProcessingResponse`. Two module groups share this tier:
//! `adapter` exercises generic transport/adapter mechanics with stub filters;
//! `guardrails` exercises real guardrail behaviour against a recording `NeMo` peer.

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
#![allow(dead_code, reason = "shared transport toolbox; each binary uses a subset")]

mod common;

#[path = "grpc/adapter/mod.rs"]
mod adapter;

#[path = "grpc/guardrails/mod.rs"]
mod guardrails;
