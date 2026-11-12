//! Successive response-map changes must survive deferred wire delivery.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    clippy::missing_assert_message,
    clippy::missing_docs_in_private_items,
    clippy::large_futures,
    clippy::future_not_send,
    reason = "test fixtures and assertions"
)]
use praxis_proto::envoy::service::{
    common::v3::{HeaderValue, HeaderValueOption, header_value_option::HeaderAppendAction},
    ext_proc::v3::{HttpBody, HttpHeaders, processing_response::Response as Reply},
};

use super::*;
use crate::handlers::{handle_response_body, handle_response_headers, handle_trailers};

struct HeaderPhases {
    buffered: bool,
}

#[async_trait::async_trait]
impl praxis_filter::HttpFilter for HeaderPhases {
    fn name(&self) -> &'static str {
        "header_phases"
    }

    fn response_body_access(&self) -> praxis_filter::BodyAccess {
        praxis_filter::BodyAccess::ReadWrite
    }

    fn response_body_mode(&self) -> praxis_filter::BodyMode {
        if self.buffered {
            praxis_filter::BodyMode::StreamBuffer { max_bytes: Some(1024) }
        } else {
            praxis_filter::BodyMode::Stream
        }
    }

    async fn on_request(&self, _: &mut HttpFilterContext<'_>) -> Result<FilterAction, praxis_filter::FilterError> {
        Ok(FilterAction::Continue)
    }

    async fn on_response(&self, ctx: &mut HttpFilterContext<'_>) -> Result<FilterAction, praxis_filter::FilterError> {
        let headers = &mut ctx.response_header.as_mut().unwrap().headers;
        for value in ["early-a", "early-b"] {
            headers.append("x-remove", value.parse().unwrap());
            headers.append("set-cookie", value.parse().unwrap());
        }
        headers.insert("x-keep", "unchanged".parse().unwrap());
        headers.insert("content-length", "999".parse().unwrap());
        Ok(FilterAction::Continue)
    }

    fn on_response_body(
        &self,
        ctx: &mut HttpFilterContext<'_>,
        body: &mut Option<Bytes>,
        _: bool,
    ) -> Result<FilterAction, praxis_filter::FilterError> {
        let headers = &mut ctx.response_header.as_mut().unwrap().headers;
        headers.remove("x-remove");
        headers.remove("set-cookie");
        headers.append("set-cookie", "final-a".parse().unwrap());
        headers.append("set-cookie", "final-b".parse().unwrap());
        *body = Some(Bytes::new());
        Ok(FilterAction::Continue)
    }
}

#[expect(clippy::unnecessary_wraps, reason = "registry signature")]
fn header_phases(config: &serde_yaml::Value) -> Result<Box<dyn praxis_filter::HttpFilter>, praxis_filter::FilterError> {
    Ok(Box::new(HeaderPhases {
        buffered: config
            .get("buffered")
            .and_then(serde_yaml::Value::as_bool)
            .unwrap_or(false),
    }))
}

#[tokio::test]
async fn fds_first_body_mutations_replace_deferred_response_header_values() {
    for buffered in [false, true] {
        for trailers in [false, true] {
            let cfg: crate::config::ExtProcConfig = serde_yaml::from_str(&format!("filter_chains:\n  - name: main\n    filters:\n      - filter: header_phases\n        buffered: {buffered}\n")).unwrap();
            let mut registry = praxis_filter::FilterRegistry::with_builtins();
            registry
                .register("header_phases", praxis_filter::http_builtin(header_phases))
                .unwrap();
            let pipeline = crate::config::build_pipeline(&cfg, &registry).unwrap();
            let mut state = StreamState::new();
            state.request = Some(adapter::envoy_headers_to_request(&[]));
            state.protocol_config.response_body_mode = BodyMode::FullDuplexStreamed;
            assert!(
                handle_response_headers(&pipeline, HttpHeaders::default(), &mut state)
                    .await
                    .unwrap()
                    .is_empty()
            );
            let mut replies = handle_response_body(
                &pipeline,
                HttpBody {
                    body: b"upstream body".to_vec(),
                    end_of_stream: !trailers,
                },
                &mut state,
            )
            .await
            .unwrap();
            if trailers {
                replies.extend(handle_trailers(&pipeline, &mut state, false).await.unwrap());
            }
            let header_replies: Vec<_> = replies
                .iter()
                .filter_map(|reply| match &reply.response {
                    Some(Reply::ResponseHeaders(h)) => Some(h),
                    _ => None,
                })
                .collect();
            assert_eq!(header_replies.len(), 1, "headers are emitted exactly once");
            assert!(matches!(replies[0].response, Some(Reply::ResponseHeaders(_))));
            let mutation = header_replies[0]
                .response
                .as_ref()
                .unwrap()
                .header_mutation
                .as_ref()
                .unwrap();
            let values = |name: &str| {
                mutation
                    .set_headers
                    .iter()
                    .filter_map(|entry| entry.header.as_ref())
                    .filter(|header| header.key.eq_ignore_ascii_case(name))
                    .map(|header| {
                        if header.raw_value.is_empty() {
                            header.value.as_bytes().to_vec()
                        } else {
                            header.raw_value.clone()
                        }
                    })
                    .collect::<Vec<_>>()
            };
            assert!(
                values("x-remove").is_empty(),
                "an early set must not resurrect a removed header: {mutation:?}"
            );
            assert!(mutation.remove_headers.iter().any(|name| name == "x-remove"));
            assert_eq!(values("set-cookie"), [b"final-a".to_vec(), b"final-b".to_vec()]);
            let cookies: Vec<_> = mutation
                .set_headers
                .iter()
                .filter(|entry| entry.header.as_ref().is_some_and(|h| h.key == "set-cookie"))
                .collect();
            assert_eq!(
                cookies[0].append_action,
                i32::from(HeaderAppendAction::OverwriteIfExistsOrAdd)
            );
            assert_eq!(
                cookies[1].append_action,
                i32::from(HeaderAppendAction::AppendIfExistsOrAdd)
            );
            assert_eq!(cookies[0].append, Some(false));
            assert_eq!(cookies[1].append, Some(true));
            assert_eq!(values("x-keep"), [b"unchanged".to_vec()]);
            if buffered {
                assert_eq!(values("content-length"), [b"0".to_vec()]);
            } else {
                assert!(values("content-length").is_empty());
                assert!(mutation.remove_headers.iter().any(|name| name == "content-length"));
            }
        }
    }
}

#[test]
fn response_composition_preserves_final_values_and_unrelated_names() {
    let set = |name: &str, value: &str, append| HeaderValueOption {
        header: Some(HeaderValue {
            key: name.into(),
            value: value.into(),
            ..Default::default()
        }),
        append_action: i32::from(if append {
            HeaderAppendAction::AppendIfExistsOrAdd
        } else {
            HeaderAppendAction::OverwriteIfExistsOrAdd
        }),
        ..Default::default()
    };
    let prior = HeaderMutation {
        set_headers: vec![
            set("X-Remove", "a", false),
            set("x-remove", "b", true),
            set("X-Replace", "old", false),
            set("x-keep", "keep", false),
        ],
        remove_headers: vec!["X-Revive".into(), "x-still-removed".into()],
    };
    let next = HeaderMutation {
        set_headers: vec![
            set("x-replace", "first", false),
            set("x-replace", "second", true),
            set("x-revive", "new", false),
        ],
        remove_headers: vec!["x-remove".into()],
    };
    let combined = compose_response_mutations(Some(prior.clone()), Some(next.clone())).unwrap();
    let mut expected = vec![set("x-keep", "keep", false)];
    expected.extend(next.set_headers.clone());
    assert_eq!(combined.set_headers, expected);
    assert_eq!(combined.remove_headers, ["x-still-removed", "x-remove"]);
    assert_eq!(compose_response_mutations(None, None), None);
    assert_eq!(compose_response_mutations(Some(prior.clone()), None), Some(prior));
    assert_eq!(compose_response_mutations(None, Some(next.clone())), Some(next));
}
