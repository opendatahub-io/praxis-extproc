# rstest candidates

Inventory of the root integration test suite, excluding the Envoy tier reserved for a separate PR. These tests can benefit from parameterization; none requires rstest for correctness. This change adds the development dependency only. Existing tests remain unchanged.

Use named `#[case::name(...)]` cases for related tuples and `#[values(...)]` for independent Cartesian dimensions. Keep `#[tokio::test]` for the async runtime. Default rstest features are disabled because these tests already use Tokio deadlines and do not rename the dependency.

## Existing matrices

41 test functions contain suitable parameter dimensions. Each row identifies a function and the loop(s) that can become parameters.

### [grpc/guardrails/mod.rs](../tests/grpc/guardrails/mod.rs)

| Test | Parameters / constraints |
| --- | --- |
| [selected_phases_call_nemo_with_complete_content_in_every_response_mode](../tests/grpc/guardrails/mod.rs) | Response mode × translation × input/output selection. |
| [streamed_output_is_withheld_until_the_complete_response_is_checked](../tests/grpc/guardrails/mod.rs) | Pass / blocked output. |
| [output_provider_failures_never_release_unvetted_content](../tests/grpc/guardrails/mod.rs) | Response mode × provider failure. |
| [output_guarded_sse_requests_are_explicitly_rejected_in_all_response_modes](../tests/grpc/guardrails/mod.rs) | Response mode × translation. |

### [grpc/guardrails/boundaries.rs](../tests/grpc/guardrails/boundaries.rs)

| Test | Parameters / constraints |
| --- | --- |
| [input_only_and_unselected_checks_release_each_sse_chunk_before_eos](../tests/grpc/guardrails/boundaries.rs) | Translation × input selection; keep chunk sequence inside each case. |
| [empty_final_body_frame_checks_and_releases_the_buffered_document](../tests/grpc/guardrails/boundaries.rs) | Response mode × verdict. |
| [server_and_filter_response_limits_fail_closed_before_provider_evaluation](../tests/grpc/guardrails/boundaries.rs) | Paired server limit / effective limit. |

### [grpc/guardrails/fds.rs](../tests/grpc/guardrails/fds.rs)

| Test | Parameters / constraints |
| --- | --- |
| [fds_input_checks_complete_chunks_at_body_eos_and_trailers](../tests/grpc/guardrails/fds.rs) | Trailers × provider reply. |
| [fds_output_trailers_release_only_the_checked_replacement_before_ack](../tests/grpc/guardrails/fds.rs) | Provider reply. |
| [unconditional_and_host_selected_fds_output_checks_are_enforced](../tests/grpc/guardrails/fds.rs) | Named pairs of host condition / selection. |
| [mixed_fds_modes_preserve_selection_and_header_body_order](../tests/grpc/guardrails/fds.rs) | Paired request/response modes × header wait × selection. |
| [cancelled_fds_provider_calls_release_resources_and_next_stream_state_is_fresh](../tests/grpc/guardrails/fds.rs) | Input/output phase only; keep cancellation and fresh stream together. |
| [headers_only_requests_and_responses_complete_without_body_callouts](../tests/grpc/guardrails/fds.rs) | Request mode × response mode. |
| [interleaved_fds_response_preserves_request_header_filter_state](../tests/grpc/guardrails/fds.rs) | Header wait. |
| [input_only_and_unselected_fds_output_streams_before_body_eos_or_trailers](../tests/grpc/guardrails/fds.rs) | Input selection × header wait × trailers. |

### [grpc/guardrails/limits.rs](../tests/grpc/guardrails/limits.rs)

| Test | Parameters / constraints |
| --- | --- |
| [selected_fds_checks_reject_response_headers_before_request_completion](../tests/grpc/guardrails/limits.rs) | Response mode × header wait × selection. |
| [unselected_fds_requests_preserve_full_duplex_interleaving](../tests/grpc/guardrails/limits.rs) | Response mode × header wait. |
| [buffered_output_enforces_the_runtime_guardrail_cap](../tests/grpc/guardrails/limits.rs) | Exact cap / overflow. |
| [fds_input_enforces_the_effective_cap_before_provider_evaluation](../tests/grpc/guardrails/limits.rs) | Paired limits × trailers × overflow. |
| [fds_output_enforces_the_effective_cap_before_provider_evaluation](../tests/grpc/guardrails/limits.rs) | Paired limits × trailers × overflow. |
| [buffered_checked_requests_allow_skipped_or_delivered_trailers](../tests/grpc/guardrails/limits.rs) | Response mode × selection × header wait × trailers. |
| [buffered_trailer_allowance_never_admits_missing_or_oversized_bodies](../tests/grpc/guardrails/limits.rs) | Missing/oversized body × selection. |

### [grpc/guardrails/selection.rs](../tests/grpc/guardrails/selection.rs)

| Test | Parameters / constraints |
| --- | --- |
| [unsupported_selector_shapes_require_complete_requests_without_streaming_overrides](../tests/grpc/guardrails/selection.rs) | Paired selector shapes × request mode. |
| [production_selector_preserves_request_mode_and_header_wait_contract](../tests/grpc/guardrails/selection.rs) | Response mode × selection × header wait. |
| [selected_guardrails_reject_incomplete_request_modes_before_forwarding](../tests/grpc/guardrails/selection.rs) | Request mode × response mode × selection × header wait. |
| [incomplete_modes_allow_unselected_and_truly_bodyless_requests](../tests/grpc/guardrails/selection.rs) | Request mode × named selection/EOS pairs. |
| [raw_header_values_are_authoritative_for_actual_guardrail_execution](../tests/grpc/guardrails/selection.rs) | Named raw value / text value / expected selection tuples. |
| [duplicate_or_malformed_selection_is_rejected_before_any_callout](../tests/grpc/guardrails/selection.rs) | Named invalid selection headers. |
| [controller_check_ids_and_output_selectors_still_reject_ambiguity](../tests/grpc/guardrails/selection.rs) | Header name × invalid selection headers. |
| [selected_output_cannot_be_silently_disabled_by_none_response_mode](../tests/grpc/guardrails/selection.rs) | Output selection. |
| [embedded_guardrail_services_require_complete_request_modes_without_a_selector](../tests/grpc/guardrails/selection.rs) | Split rejection and acceptance matrices into separate tests first; then parameterize each. |
| [unknown_guardrail_selection_explains_premature_response_rejection](../tests/grpc/guardrails/selection.rs) | Embedded service × header wait. |

### [grpc/guardrails/stream_rejection.rs](../tests/grpc/guardrails/stream_rejection.rs)

| Test | Parameters / constraints |
| --- | --- |
| [immediate_response_ends_stream_after_first_or_later_chunk_rejection](../tests/grpc/guardrails/stream_rejection.rs) | Mode × accepted chunk count × rejection at EOS; keep chunk sequence inside each case. |

### [grpc/guardrails/payload_boundaries.rs](../tests/grpc/guardrails/payload_boundaries.rs)

| Test | Parameters / constraints |
| --- | --- |
| [guarded_json_boundary_matrix_preserves_content_and_rejects_before_nemo](../tests/grpc/guardrails/payload_boundaries.rs) | Phase × seed × framing. Retain ending/size sequences within the shared harness initially; splitting all dimensions would create 162 independently scheduled cases. |

### [grpc/adapter/limits.rs](../tests/grpc/adapter/limits.rs)

| Test | Parameters / constraints |
| --- | --- |
| [streamed_body_filter_enforces_cumulative_limit_before_the_next_hook](../tests/grpc/adapter/limits.rs) | Overflow; retain cumulative chunk sequence within each case. |

### [grpc/adapter/message_limits.rs](../tests/grpc/adapter/message_limits.rs)

| Test | Parameters / constraints |
| --- | --- |
| [grpc_receive_boundary_counts_serialized_protobuf_not_only_body_bytes](../tests/grpc/adapter/message_limits.rs) | Receive limit relative to encoded length: −1 / exact / +1. |

### [grpc/adapter/payloads.rs](../tests/grpc/adapter/payloads.rs)

| Test | Parameters / constraints |
| --- | --- |
| [splits_inside_utf8_scalars_and_json_escapes_preserve_body_filter_content](../tests/grpc/adapter/payloads.rs) | Named split locations: two emoji byte boundaries, escaped quote, escaped newline. |
| [unguarded_thirty_two_mib_json_boundaries_recover_on_fresh_streams](../tests/grpc/adapter/payloads.rs) | Framing only; retain the ordered boundary and recovery sizes on one channel. |

### [grpc/adapter/streaming.rs](../tests/grpc/adapter/streaming.rs)

| Test | Parameters / constraints |
| --- | --- |
| [sse_fragments_reassemble_with_exactly_one_terminal_done_marker](../tests/grpc/adapter/streaming.rs) | Streamed / full duplex streamed; retain fragment sequence. |

### [grpc/adapter/validation.rs](../tests/grpc/adapter/validation.rs)

| Test | Parameters / constraints |
| --- | --- |
| [invalid_listener_addresses_fail_validate](../tests/grpc/adapter/validation.rs) | gRPC / health / metrics address flag. |

### [deploy_contract/envoy_filter.rs](../tests/deploy_contract/envoy_filter.rs)

| Test | Parameters / constraints |
| --- | --- |
| [epp_runs_after_payload_processing_on_every_kuadrant_variant](../tests/deploy_contract/envoy_filter.rs) | Named filter-chain / auth anchor pairs. |

## Optional consolidation of separate tests

These already have individually reported results. Converting them is useful only if reducing duplicate setup is worthwhile.

| File | Tests | Potential cases |
| --- | --- | --- |
| [adapter/basics.rs](../tests/grpc/adapter/basics.rs) | `guardrails_filter_rejects_blocked_content`, `guardrails_filter_allows_clean_content` | Content / expected verdict. |
| [adapter/sequencing.rs](../tests/grpc/adapter/sequencing.rs) | `initial_request_body_rejected`, `initial_request_trailers_rejected` | Initial message / expected status; retain independent expectations. |
| [adapter/limits.rs](../tests/grpc/adapter/limits.rs) | `buffered_body_at_and_over_max_body_bytes` | Exact cap / overflow, if shared-channel execution is not needed. |
| [conformance/direct.rs](../tests/k8s_e2e/conformance/direct.rs) | `direct_external_returns_200`, `direct_internal_returns_200`; `direct_external_has_praxis_mutations`, `direct_internal_has_praxis_mutations` | External/internal route and model; consolidate each pair separately. |
| [conformance/guardrails.rs](../tests/k8s_e2e/conformance/guardrails.rs) | `provider_failure_fails_closed_on_response`, `provider_timeout_fails_closed_on_response`, `provider_malformed_response_fails_closed_on_response`, `provider_unknown_status_fails_closed_on_response` | Named provider failure tokens, all using the same existing assertion helper. |

## Keep sequences and assertions together

- [adapter/payloads.rs](../tests/grpc/adapter/payloads.rs): `body_cap_boundaries_preserve_content_and_recover_on_the_same_channel` deliberately sends cap−1, cap, cap+1, cap−1 on the same channel. The last request proves recovery.
- [adapter/streaming.rs](../tests/grpc/adapter/streaming.rs): `full_duplex_request_and_response_body_chunks_interleave_before_either_eos` is one ordered exchange, with no independent parameter dimension.
- [adapter/lifecycle.rs](../tests/grpc/adapter/lifecycle.rs), [adapter/basics.rs](../tests/grpc/adapter/basics.rs), and [guardrails/boundaries.rs](../tests/grpc/guardrails/boundaries.rs): concurrency and stream-isolation loops deliberately share a connection or overlap tasks.
- [guardrails/provider_content.rs](../tests/grpc/guardrails/provider_content.rs): iterating over recorded provider calls checks one exchange; these are assertions, not independent cases.
- [services.rs](../tests/services.rs): metrics loops deliberately accumulate values before checking a shared snapshot.
- Helper loops that encode bytes, send fragments, apply configuration patches, inspect headers, or poll readiness remain ordinary loops.

## Conversion considerations

Moving a loop to rstest changes scheduling and setup frequency. Preserve the harness scope intentionally, especially for large payload matrices. A fully expanded matrix creates independently scheduled tests and can increase memory usage. Do not split a recovery proof or cancellation/fresh-stream proof across separate cases.

No conversion is proposed for the remaining TLS, FIPS, service, or Kubernetes scenarios: their different behavior and assertions already justify separate tests.
