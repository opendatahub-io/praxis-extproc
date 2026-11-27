// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Red Hat, Inc.

//! Duplicate header values and cumulative pipeline mutations.

use super::support::*;

#[tokio::test]
async fn duplicate_request_header_keys_preserve_every_value_in_wire_order() {
    let (mut client, _shutdown) = start_server_with_registry(PROBE_CONFIG, probe_registry()).await;
    let (tx, mut stream) = open_stream(&mut client).await;
    let headers = make_request_headers_with(
        "GET",
        "/duplicates",
        true,
        [
            make_header("x-duplicate", "first"),
            HeaderValue {
                key: "x-duplicate".to_owned(),
                raw_value: b"second".to_vec(),
                ..Default::default()
            },
            make_header("x-duplicate", "third"),
        ],
    );
    tx.send(headers).await.unwrap();
    drop(tx);
    let replies = finish(&mut stream).await;
    let headers = extract_all_set_headers(&replies);
    assert!(
        headers
            .iter()
            .any(|header| { header.key == "x-observed-duplicates" && header.value == "first|second|third" })
    );
}


#[tokio::test]
async fn multiple_filters_accumulate_header_and_body_mutations_in_order() {
    let (mut client, _shutdown) = start_server_with_registry(CHAIN_CONFIG, probe_registry()).await;
    let replies = send_full_request(&mut client, "POST", "/chain", b"payload").await;
    assert_eq!(request_bytes(&replies), b"payload:first:second");
    let headers = extract_all_set_headers(&replies);
    for stage in ["first", "second"] {
        assert!(
            headers
                .iter()
                .any(|header| header.key == format!("x-{stage}") && header.value == stage)
        );
    }
}
