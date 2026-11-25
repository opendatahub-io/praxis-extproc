// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Red Hat, Inc.

//! Exact payload bytes, fragmented-body accumulation, and body-cap recovery.

use std::sync::{Arc, Mutex};

use bytes::Bytes;
use praxis_filter::FilterFactory;
use serde_json::json;

use super::support::*;

// -----------------------------------------------------------------------------
// Constants
// -----------------------------------------------------------------------------

const CHUNK_BYTES: usize = 16_384; // 16 KiB.
const FILTER_BODY_CAP: usize = 33_554_432; // 32 MiB.
const PAYLOAD_BYTES: usize = 65_536; // 64 KiB.
const PAYLOAD_SEED: &str = "café ☕ \"q\"\n";

#[tokio::test]
async fn exact_size_multibyte_payload_reaches_body_filter_without_truncation() {
    let (mut client, calls, _shutdown) = recording_server(PAYLOAD_BYTES).await;
    let body = exact_size_body(PAYLOAD_BYTES);
    assert_eq!(body.len(), PAYLOAD_BYTES);
    assert_reassembled(&mut client, &calls, &body, vec![body.as_slice()]).await;
}

#[tokio::test]
async fn sixteen_kib_chunks_reassemble_byte_exact_at_body_filter() {
    let (mut client, calls, _shutdown) = recording_server(PAYLOAD_BYTES + 17).await;
    let body = exact_size_body(PAYLOAD_BYTES + 17);
    let chunks: Vec<_> = body.chunks(CHUNK_BYTES).collect();
    assert_eq!(chunks.len(), 5);
    assert_reassembled(&mut client, &calls, &body, chunks).await;
}

#[tokio::test]
async fn splits_inside_utf8_scalars_and_json_escapes_preserve_body_filter_content() {
    let (mut client, calls, _shutdown) = recording_server(1024).await;
    let body = exact_size_body(1024);
    let scalar = body.windows(3).position(|bytes| bytes == "☕".as_bytes()).unwrap();
    let quote = body.windows(2).position(|bytes| bytes == br#"\""#).unwrap();
    let newline = body.windows(2).position(|bytes| bytes == br"\n").unwrap();
    for split in [scalar + 1, scalar + 2, quote + 1, newline + 1] {
        let (first, last) = body.split_at(split);
        assert_reassembled(&mut client, &calls, &body, vec![first, last]).await;
    }
}

#[tokio::test]
async fn body_cap_boundaries_preserve_content_and_recover_on_the_same_channel() {
    let cap = 1024;
    let (mut client, calls, _shutdown) = recording_server(cap).await;
    for length in [cap - 1, cap, cap + 1, cap - 1] {
        let body = exact_size_body(length);
        let before = calls.lock().unwrap().len();
        let replies = tokio::time::timeout(DEADLINE, async {
            let (tx, mut stream) = open_stream(&mut client).await;
            tx.send(make_request_headers("POST", "/cap", false)).await.unwrap();
            assert!(matches!(
                next(&mut stream).await.response,
                Some(RespVariant::RequestHeaders(_))
            ));
            tx.send(request_body(&body, true)).await.unwrap();
            drop(tx);
            finish(&mut stream).await
        })
        .await
        .expect("body-cap exchange must open, send, and finish before deadline");
        if length > cap {
            assert_eq!(immediate_status(&replies), Some(413));
            assert_eq!(
                calls.lock().unwrap().len(),
                before,
                "oversized input must not reach a body hook"
            );
        } else {
            assert_eq!(immediate_status(&replies), None);
            assert_eq!(request_bytes(&replies), body);
            let calls = calls.lock().unwrap();
            assert_eq!(calls.len(), before + 1);
            assert_eq!(calls[before], body);
            drop(calls);
        }
    }
}

#[tokio::test]
async fn unguarded_thirty_two_mib_json_boundaries_recover_on_fresh_streams() {
    use crate::common::payloads::{Framing, Phase, UNGUARDED_CAP, exact_document, fragments};

    let (mut client, calls, _shutdown) = recording_server(UNGUARDED_CAP).await;
    for framing in [Framing::Single, Framing::Chunks] {
        for size in [UNGUARDED_CAP - 1, UNGUARDED_CAP, UNGUARDED_CAP + 1, 1024] {
            let (body, _) = exact_document(Phase::Input, size, "café ☕ \"q\"\n");
            if size <= UNGUARDED_CAP {
                assert_reassembled(&mut client, &calls, &body, fragments(&body, framing)).await;
                calls.lock().unwrap().clear();
            } else {
                let replies = tokio::time::timeout(DEADLINE, async {
                    let (tx, mut stream) = open_stream(&mut client).await;
                    let mut headers = make_request_headers("POST", "/oversized", false);
                    headers.protocol_config = Some(protocol(BodySendMode::FullDuplexStreamed, BodySendMode::Buffered));
                    tx.send(headers).await.unwrap();
                    let chunks = fragments(&body, framing);
                    for (index, chunk) in chunks.iter().enumerate() {
                        tx.send(request_body(chunk, index + 1 == chunks.len())).await.unwrap();
                    }
                    drop(tx);
                    finish(&mut stream).await
                })
                .await
                .expect("oversized payload exchange must open, send, and finish before deadline");
                assert_eq!(immediate_status(&replies), Some(413));
                assert!(request_bytes(&replies).is_empty());
                assert!(
                    calls.lock().unwrap().is_empty(),
                    "oversized JSON must never reach the hook"
                );
            }
        }
    }
}

// -----------------------------------------------------------------------------
// Test Utilities
// -----------------------------------------------------------------------------

type RecordedBodies = Arc<Mutex<Vec<Vec<u8>>>>;

async fn recording_server(cap: usize) -> (ExtProcClient, RecordedBodies, tokio::sync::oneshot::Sender<()>) {
    let calls = RecordedBodies::default();
    let record = Arc::clone(&calls);
    let mut registry = FilterRegistry::with_builtins();
    registry
        .register(
            "record_body",
            FilterFactory::Http(Arc::new(move |_| {
                Ok(Box::new(RecordingBody {
                    calls: Arc::clone(&record),
                }))
            })),
        )
        .unwrap();
    let yaml = format!(
        "server:\n  max_body_bytes: {cap}\nfilter_chains:\n  - name: main\n    filters:\n      - filter: record_body\n"
    );
    let (client, shutdown) = start_server_with_registry(&yaml, registry).await;
    (client, calls, shutdown)
}

struct RecordingBody {
    calls: RecordedBodies,
}

#[async_trait::async_trait]
impl HttpFilter for RecordingBody {
    fn name(&self) -> &'static str {
        "record_body"
    }

    async fn on_request(&self, _: &mut HttpFilterContext<'_>) -> Result<FilterAction, FilterError> {
        Ok(FilterAction::Continue)
    }

    fn request_body_access(&self) -> BodyAccess {
        BodyAccess::ReadWrite
    }

    fn request_body_mode(&self) -> FilterBodyMode {
        FilterBodyMode::StreamBuffer {
            max_bytes: Some(FILTER_BODY_CAP),
        }
    }

    async fn on_request_body(
        &self,
        _: &mut HttpFilterContext<'_>,
        body: &mut Option<Bytes>,
        end_of_stream: bool,
    ) -> Result<FilterAction, FilterError> {
        assert!(end_of_stream, "a buffered filter must receive the complete body");
        self.calls
            .lock()
            .unwrap()
            .push(body.as_deref().unwrap_or_default().to_vec());
        Ok(FilterAction::Continue)
    }
}

async fn assert_reassembled(client: &mut ExtProcClient, calls: &RecordedBodies, body: &[u8], chunks: Vec<&[u8]>) {
    let before = calls.lock().unwrap().len();
    let replies = tokio::time::timeout(DEADLINE, async {
        let (tx, mut stream) = open_stream(client).await;
        let mut headers = make_request_headers("POST", "/payload", false);
        headers.protocol_config = Some(protocol(BodySendMode::FullDuplexStreamed, BodySendMode::Buffered));
        tx.send(headers).await.unwrap();
        let last = chunks.len() - 1;
        for (index, chunk) in chunks.into_iter().enumerate() {
            tx.send(request_body(chunk, index == last)).await.unwrap();
        }
        drop(tx);
        finish(&mut stream).await
    })
    .await
    .expect("payload exchange must open, send, and finish before deadline");
    assert_eq!(immediate_status(&replies), None);
    assert_eq!(
        request_bytes(&replies),
        body,
        "forwarded bytes must equal the input JSON"
    );
    let endings: Vec<_> = replies.iter().filter_map(streamed_eos).collect();
    assert_eq!(endings.iter().filter(|&&eos| eos).count(), 1);
    assert_eq!(endings.last(), Some(&true));
    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), before + 1, "one body hook for the complete document");
    assert_eq!(calls[before], body, "the body filter must receive the exact wire bytes");
    drop(calls);
}

fn exact_size_body(target: usize) -> Vec<u8> {
    let envelope = |text: &str| json!({"text": text});
    let overhead = serde_json::to_vec(&envelope("")).unwrap().len();
    let base = serde_json::to_vec(&envelope(PAYLOAD_SEED)).unwrap().len();
    assert!(target >= base, "target {target} must hold the envelope and seed");
    let repetitions = (target - overhead) / (base - overhead);
    let mut content = PAYLOAD_SEED.repeat(repetitions);
    let remainder = (target - overhead) % (base - overhead);
    content.push_str(&"a".repeat(remainder));
    let bytes = serde_json::to_vec(&envelope(&content)).unwrap();
    assert_eq!(bytes.len(), target, "exact-size payload must hit its byte target");
    bytes
}
