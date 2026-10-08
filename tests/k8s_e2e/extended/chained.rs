//! Per-hop body integrity on the `MaaS` two-hop chain.

use super::{
    helpers::{
        HOP_DIGEST_HEADER_1, HOP_DIGEST_HEADER_2, assert_hop_digests, chat_completion_with_digest,
        load_topology_profile,
    },
    report::ExtendedRunMetadata,
};
use crate::fixtures::{assert_praxis_mutations, ensure_gateway_ready};

#[tokio::test]
#[ignore = "qualification tier: requires make e2e-setup-extended"]
async fn chained_two_hop_oracle_matches_client_digest() {
    ensure_gateway_ready().await;
    let profile = load_topology_profile();
    assert_eq!(
        profile.name, "maas_two_hop_fd_streamed",
        "chained scenarios run on default MaaS profile"
    );
    assert_eq!(profile.ext_proc_hops, 2, "expected two ext-proc hops");
    assert_eq!(
        profile.ext_proc_modes.request, "FULL_DUPLEX_STREAMED",
        "topology request mode"
    );
    assert_eq!(
        profile.ext_proc_modes.response, "FULL_DUPLEX_STREAMED",
        "topology response mode"
    );
    assert_eq!(profile.tls, "tls", "topology tls mode");

    let (resp, digest) = chat_completion_with_digest("gpt-4", "extended chained oracle").await;
    assert_eq!(resp.status(), 200, "chained chat completion should succeed");
    assert_praxis_mutations(&resp);
    assert_hop_digests(&digest, resp.headers());

    ExtendedRunMetadata::publish_chain_executed(&profile.name, "maas pre-ipp and post-ipp ext-proc hops")
        .expect("publish chain=executed metadata");
}

#[tokio::test]
#[ignore = "qualification tier: requires make e2e-setup-extended"]
async fn chained_hop_headers_present() {
    ensure_gateway_ready().await;
    let (resp, digest) = chat_completion_with_digest("gpt-4", "hop header presence").await;
    assert_eq!(resp.status(), 200, "chat completion should return 200");
    let headers = resp.headers();
    assert!(headers.get(HOP_DIGEST_HEADER_1).is_some(), "hop 1 header missing");
    assert!(headers.get(HOP_DIGEST_HEADER_2).is_some(), "hop 2 header missing");
    assert_hop_digests(&digest, headers);
}
