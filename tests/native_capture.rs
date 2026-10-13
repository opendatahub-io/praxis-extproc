// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Red Hat, Inc.

//! Native JSON capture and streaming transport over the gRPC boundary.

#[cfg(test)]
#[cfg(feature = "responses-full")]
#[path = "support/native_capture.rs"]
mod capture;

#[cfg(test)]
#[cfg(feature = "responses-full")]
#[expect(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    reason = "integration tests"
)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use praxis_ai_apis::{
        StateOwner,
        store::{DEFAULT_STORE_NAME, ResponseStoreRegistry, SqliteResponseStore},
    };
    use praxis_extproc::server::PraxisExtProc;
    use praxis_filter::FilterPipeline;
    use praxis_proto::envoy::service::ext_proc::v3::{
        HttpBody, ProcessingRequest, ProcessingResponse, body_mutation,
        external_processor_client::ExternalProcessorClient, external_processor_server::ExternalProcessorServer,
        processing_request::Request, processing_response::Response,
    };
    use serde_json::json;
    use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
    use tonic::transport::{Channel, Server};

    use super::capture::{capture_headers, native_capture_phases, native_exchange};

    #[tokio::test(flavor = "multi_thread")]
    async fn native_capture_survives_body_modes_and_empty_eos() {
        for (mode, empty_eos) in [(2, false), (1, false), (1, true)] {
            let (pipeline, store, registry) = capture_pipeline().await;
            let owner = StateOwner::from_trusted_parts("tenant", "test", "alice").unwrap();
            let scoped = registry.get_scoped(DEFAULT_STORE_NAME, &owner).unwrap();
            let (mut client, shutdown, task) = start_service(PraxisExtProc::new(pipeline)).await;
            let replies = native_exchange(&mut client, mode, native_capture_phases(empty_eos))
                .await
                .unwrap();
            if mode == 1 {
                assert_eq!(replies[2].mode_override.as_ref().unwrap().response_body_mode, 2);
            }
            let stored = scoped.get_response("resp_native_test").await.unwrap().unwrap();
            assert_eq!(stored.input, json!("hello"));
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(body_bytes(replies.last().unwrap())).unwrap(),
                stored.response_object
            );
            let path = "/v1/responses/resp_native_test";
            for (method, path, subject, expected) in [
                ("GET", path.to_owned(), "alice", 200),
                ("GET", path.to_owned(), "bob", 404),
                ("GET", format!("{path}/input_items"), "alice", 200),
                ("DELETE", path.to_owned(), "bob", 404),
                ("GET", path.to_owned(), "alice", 200),
                ("DELETE", path.to_owned(), "alice", 200),
                ("GET", path.to_owned(), "alice", 404),
            ] {
                assert_eq!(local_status(&mut client, method, &path, subject).await, expected);
            }
            shutdown.send(()).unwrap();
            task.await.unwrap().unwrap();
            store.close().await;
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn native_sse_keeps_incremental_bytes() {
        let (pipeline, store, _) = capture_pipeline().await;
        let (mut client, shutdown, task) = start_service(PraxisExtProc::new(pipeline)).await;
        let mut phases = native_capture_phases(true);
        phases[1] = Request::RequestBody(HttpBody {
            body: br#"{"model":"test","input":"hello","store":true,"stream":true}"#.to_vec(),
            end_of_stream: true,
        });
        phases[2] = Request::ResponseHeaders(capture_headers(&[
            (":status", "200"),
            ("content-type", "text/event-stream"),
        ]));
        let chunk = b": keep-alive\n\n";
        phases[3] = Request::ResponseBody(HttpBody {
            body: chunk.to_vec(),
            end_of_stream: false,
        });
        let replies = native_exchange(&mut client, 1, phases).await.unwrap();
        assert_eq!(replies.len(), 5);
        assert!(replies.iter().all(|reply| reply.mode_override.is_none()));
        assert_eq!(body_bytes(&replies[3]), chunk, "SSE bytes arrive before EOS");
        assert!(body_bytes(&replies[4]).is_empty(), "EOS must not replay prior bytes");
        shutdown.send(()).unwrap();
        task.await.unwrap().unwrap();
        store.close().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn failed_storage_prevents_success_body_release() {
        let (pipeline, store, _) = capture_pipeline().await;
        let (mut client, shutdown, task) = start_service(PraxisExtProc::new(pipeline)).await;
        store.close().await;
        let error = native_exchange(&mut client, 1, native_capture_phases(true))
            .await
            .unwrap_err();
        assert_eq!(error.code(), tonic::Code::Internal);
        shutdown.send(()).unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oversized_json_fails_before_persistence() {
        let (pipeline, store, registry) = capture_pipeline().await;
        let service = PraxisExtProc::new(pipeline).with_max_body_accumulation(Some(64));
        let (mut client, shutdown, task) = start_service(service).await;
        let error = native_exchange(&mut client, 1, native_capture_phases(true))
            .await
            .unwrap_err();
        assert_eq!(error.code(), tonic::Code::ResourceExhausted);
        let owner = StateOwner::from_trusted_parts("tenant", "test", "alice").unwrap();
        assert!(
            registry
                .get_scoped(DEFAULT_STORE_NAME, &owner)
                .unwrap()
                .get_response("resp_native_test")
                .await
                .unwrap()
                .is_none()
        );
        shutdown.send(()).unwrap();
        task.await.unwrap().unwrap();
        store.close().await;
    }

    /// Initialize storage explicitly so capture tests do not depend on bootstrap.
    async fn capture_pipeline() -> (Arc<FilterPipeline>, Arc<SqliteResponseStore>, ResponseStoreRegistry) {
        let store = Arc::new(
            SqliteResponseStore::new("sqlite::memory:", "responses", "conversations", None, None, None)
                .await
                .unwrap(),
        );
        let registry = ResponseStoreRegistry::new();
        registry
            .register(
                &Arc::from(DEFAULT_STORE_NAME),
                Arc::<SqliteResponseStore>::clone(&store),
            )
            .unwrap();
        registry.mark_ready();
        let mut entries: Vec<praxis_core::config::FilterEntry> = serde_yaml::from_str(
            "- filter: state_owner
  mode: trusted_headers
  tenant: {static: tenant}
  issuer: {static: test}
  subject: {header: x-test-user}
- filter: ai_operation
- filter: openai_responses_request
- filter: openai_responses_store
  backend: sqlite
  database_url: 'sqlite::memory:'
  responses_table: responses
  conversations_table: conversations
- filter: openai_responses_rehydrate
- filter: openai_responses_proxy
",
        )
        .unwrap();
        let mut pipeline = FilterPipeline::build(&mut entries, &praxis_ai_filters::build_ai_registry()).unwrap();
        pipeline.add_pipeline_extension(Box::new(registry.clone()));
        (Arc::new(pipeline), store, registry)
    }

    fn body_bytes(reply: &ProcessingResponse) -> &[u8] {
        let Some(Response::ResponseBody(body)) = &reply.response else {
            unreachable!("expected body acknowledgement");
        };
        match body
            .response
            .as_ref()
            .unwrap()
            .body_mutation
            .as_ref()
            .and_then(|mutation| mutation.mutation.as_ref())
        {
            Some(body_mutation::Mutation::Body(bytes)) => bytes,
            None => &[],
            _ => unreachable!("unexpected body mutation"),
        }
    }

    async fn start_service(
        service: PraxisExtProc,
    ) -> (
        ExternalProcessorClient<Channel>,
        tokio::sync::oneshot::Sender<()>,
        tokio::task::JoinHandle<Result<(), tonic::transport::Error>>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (shutdown, receiver) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(
            Server::builder()
                .add_service(ExternalProcessorServer::new(service))
                .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async {
                    drop(receiver.await);
                }),
        );
        let client = ExternalProcessorClient::connect(format!("http://{address}"))
            .await
            .unwrap();
        (client, shutdown, task)
    }

    async fn local_status(
        client: &mut ExternalProcessorClient<Channel>,
        method: &str,
        path: &str,
        subject: &str,
    ) -> i32 {
        let message = ProcessingRequest {
            request: Some(Request::RequestHeaders(
                praxis_proto::envoy::service::ext_proc::v3::HttpHeaders {
                    end_of_stream: true,
                    ..capture_headers(&[(":method", method), (":path", path), ("x-test-user", subject)])
                },
            )),
            ..Default::default()
        };
        let mut stream = client
            .process(ReceiverStream::new({
                let (sender, receiver) = tokio::sync::mpsc::channel(1);
                sender.send(message).await.unwrap();
                receiver
            }))
            .await
            .unwrap()
            .into_inner();
        let reply = tokio::time::timeout(Duration::from_secs(5), stream.message())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let Some(Response::ImmediateResponse(response)) = reply.response else {
            unreachable!("expected local response");
        };
        response.status.unwrap().code
    }
}
