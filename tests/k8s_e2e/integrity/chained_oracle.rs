//! Per-hop body integrity on the pre-IPP → IPP two-hop chain.

use crate::{
    fixtures::{assert_praxis_mutations, ensure_gateway_ready},
    oracle_helpers::{HOP_DIGEST_HEADER_1, HOP_DIGEST_HEADER_2, assert_hop_digests, chat_completion_with_digest},
};

#[tokio::test]
async fn chained_two_hop_oracle_matches_client_digest() {
    ensure_gateway_ready().await;
    let (resp, digest) = chat_completion_with_digest("gpt-4", "chained oracle").await;
    assert_eq!(resp.status(), 200, "chained chat completion should succeed");
    assert_praxis_mutations(&resp);
    assert_hop_digests(&digest, resp.headers());
}

#[tokio::test]
async fn chained_hop_headers_present() {
    ensure_gateway_ready().await;
    let (resp, digest) = chat_completion_with_digest("gpt-4", "hop header presence").await;
    assert_eq!(resp.status(), 200, "chat completion should return 200");
    let headers = resp.headers();
    assert!(headers.get(HOP_DIGEST_HEADER_1).is_some(), "hop 1 header missing");
    assert!(headers.get(HOP_DIGEST_HEADER_2).is_some(), "hop 2 header missing");
    assert_hop_digests(&digest, headers);
}
