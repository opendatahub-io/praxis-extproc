// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Red Hat, Inc.

//! Complete decoded messages delivered to the `NeMo` provider by selected checks.

use super::{BodySendMode, support::*};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn selected_input_and_output_send_complete_decoded_messages_to_nemo() {
    let harness = Harness::new(NemoReply::Pass, false).await;
    let prompt = "café ☕ \"input\"\n".repeat(4096);
    let answer = "réponse ☕ \"output\"\n".repeat(4096);
    let replies = harness
        .complete(BodySendMode::Buffered, (true, true), &prompt, &answer)
        .await;
    assert_eq!(immediate_status(&replies), None);
    let calls = harness.calls();
    assert_eq!(calls.len(), 2, "one complete provider evaluation per selected phase");
    for (call, phase, expected) in [(&calls[0], "input", &prompt), (&calls[1], "output", &answer)] {
        assert_eq!(call["guardrails"]["rail_types"][0], phase);
        assert_eq!(call["messages"][0]["content"].as_str().unwrap(), expected);
    }
}
