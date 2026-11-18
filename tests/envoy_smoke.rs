// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Red Hat, Inc.

//! Tier 2 (thin smoke): real Envoy in front of the real ext-proc, a few cases.
//!
//! A fidelity check that the Tier-1 contracts still hold when the wire messages
//! come from a real Envoy rather than being hand-crafted, so drift between the
//! two is caught. Not a coverage tier. Gated on the `integration` feature and
//! an `ENVOY_BIN`; run via `make test-envoy`. Reuses the Tier-1 `support`
//! module (the recording `NeMo` peer) inside its own Envoy `World` harness.

#![cfg(feature = "integration")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::tests_outside_test_module,
    clippy::too_many_lines,
    clippy::missing_assert_message,
    clippy::missing_docs_in_private_items,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::future_not_send,
    clippy::large_futures,
    dead_code,
    missing_docs,
    reason = "Tier-2 smoke harness reusing the shared Tier-1 support module"
)]

#[path = "guardrails_transport/support.rs"]
mod support;

use serde_json::json;
use support::{Harness, NemoReply};

#[path = "guardrails_transport/envoy.rs"]
mod envoy;
