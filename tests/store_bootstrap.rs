// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Red Hat, Inc.

//! Local store provisioning and gRPC stored-content integration tests.

#[cfg(feature = "responses-full")]
#[cfg(test)]
#[expect(clippy::unwrap_used, clippy::too_many_lines, reason = "integration tests")]
mod tests {
    use std::{sync::Arc, time::Duration};

    use praxis_ai_apis::store::{CONVERSATIONS_STORE_NAME, DEFAULT_STORE_NAME, ResponseStoreRegistry};
    use praxis_ai_store::{ConversationRecord, ResponseRecord, StateOwner};
    use praxis_extproc::{
        config::{self, ExtProcConfig},
        server::PraxisExtProc,
    };
    use praxis_filter::{FilterPipeline, RequestExtensions};
    use praxis_proto::envoy::service::{
        common::v3::HeaderValue,
        ext_proc::v3::{
            HeaderMap, HttpBody, HttpHeaders, ProcessingRequest, external_processor_client::ExternalProcessorClient,
            external_processor_server::ExternalProcessorServer, processing_request::Request,
            processing_response::Response,
        },
    };
    use serde_json::json;
    use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
    use tonic::transport::{Channel, Server};

    #[tokio::test]
    async fn sqlite_bootstrap_serves_both_apis_and_retires_after_drain() {
        let config = store_config("sqlite::memory:");
        let (pipeline, lease) = config::build_initialized_pipeline(&config, &praxis_ai_filters::build_ai_registry())
            .await
            .unwrap();
        let registry = registry(&pipeline);
        assert!(registry.is_ready(), "only a fully initialized registry may be served");
        let owner = StateOwner::from_trusted_parts("tenant", "test", "alice").unwrap();
        let responses = registry.get_scoped(DEFAULT_STORE_NAME, &owner).unwrap();
        let conversations = registry.get_scoped(CONVERSATIONS_STORE_NAME, &owner).unwrap();
        responses
            .upsert_response(&ResponseRecord {
                id: "resp_test".to_owned(),
                owner: owner.clone(),
                created_at: 1,
                model: "test".to_owned(),
                response_object: json!({"id": "resp_test", "object": "response"}),
                input: json!([]),
                messages: json!([]),
            })
            .await
            .unwrap();
        conversations
            .upsert_conversation(&ConversationRecord {
                conversation_id: "conv_test".to_owned(),
                owner,
                created_at: 1,
                metadata: json!({}),
                messages: json!([]),
            })
            .await
            .unwrap();
        assert!(
            responses.get_conversation("conv_test").await.unwrap().is_some(),
            "compatible in-memory stores must share a backend, not separate databases"
        );

        let (mut client, shutdown, task) = start_server(Arc::clone(&pipeline)).await;
        for path in ["/v1/responses/resp_test", "/v1/conversations/conv_test"] {
            assert_eq!(
                get_status(&mut client, path, "alice").await,
                200,
                "owner must retrieve {path}"
            );
            assert_eq!(
                get_status(&mut client, path, "bob").await,
                404,
                "other owner must not retrieve {path}"
            );
        }
        shutdown.send(()).unwrap();
        task.await.unwrap().unwrap();
        drop(pipeline);
        lease.release().await;
        assert!(
            responses.get_response("resp_test").await.is_err(),
            "last lease release must close the SQL pool"
        );
    }

    #[tokio::test]
    async fn offline_pipeline_rejects_unprovisioned_stored_content() {
        let pipeline = config::build_pipeline(
            &store_config("sqlite::memory:"),
            &praxis_ai_filters::build_ai_registry(),
        )
        .unwrap();
        assert!(
            !registry(&pipeline).is_ready(),
            "offline validation must not initialize stores"
        );
        let (mut client, shutdown, task) = start_server(pipeline).await;
        for path in ["/v1/responses/resp_test", "/v1/conversations/conv_test"] {
            assert_eq!(
                get_status(&mut client, path, "alice").await,
                500,
                "unprovisioned store must fail closed: {path}"
            );
        }
        shutdown.send(()).unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn unavailable_sqlite_prevents_initialized_pipeline() {
        let config = store_config("sqlite:///proc/praxis-extproc-test/absent.db?mode=rwc");
        assert!(
            config::build_pipeline(&config, &praxis_ai_filters::build_ai_registry()).is_ok(),
            "offline validation must not connect or create a file"
        );
        let result = config::build_initialized_pipeline(&config, &praxis_ai_filters::build_ai_registry()).await;
        assert!(result.is_err(), "unavailable storage must prevent serving a pipeline");
    }

    /// Stateful configuration without inference or external network requests.
    fn store_config(url: &str) -> ExtProcConfig {
        serde_yaml::from_str(&format!(
            "
filter_chains:
  - name: main
    filters:
      - filter: state_owner
        mode: trusted_headers
        tenant: {{static: tenant}}
        issuer: {{static: test}}
        subject: {{header: x-test-user}}
      - filter: ai_operation
      - filter: openai_conversations
        backend: sqlite
        database_url: '{url}'
        conversations_table: conversations
        items_table: items
      - filter: openai_responses_store
        backend: sqlite
        database_url: '{url}'
        responses_table: responses
        conversations_table: conversations
"
        ))
        .unwrap()
    }

    /// Registry observed by filters through pipeline request extensions.
    fn registry(pipeline: &FilterPipeline) -> ResponseStoreRegistry {
        let mut extensions = RequestExtensions::new();
        pipeline.prepare_extensions(&mut extensions);
        extensions.get::<ResponseStoreRegistry>().unwrap().clone()
    }

    /// Ephemeral loopback gRPC server with awaited shutdown.
    async fn start_server(
        pipeline: Arc<FilterPipeline>,
    ) -> (
        ExternalProcessorClient<Channel>,
        tokio::sync::oneshot::Sender<()>,
        tokio::task::JoinHandle<Result<(), tonic::transport::Error>>,
    ) {
        start_service(PraxisExtProc::new(pipeline)).await
    }

    /// Run a service with explicit limits on an ephemeral loopback listener.
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

    /// Read a local stored-content response over Envoy's ExtProc protocol.
    async fn get_status(client: &mut ExternalProcessorClient<Channel>, path: &str, subject: &str) -> i32 {
        get_status_with_header(client, path, "x-test-user", subject).await
    }

    /// The gRPC boundary receives only the username Authorino has authenticated.
    async fn get_status_with_header(
        client: &mut ExternalProcessorClient<Channel>,
        path: &str,
        header: &str,
        subject: &str,
    ) -> i32 {
        local_request(client, "GET", path, (header, subject), None).await.0
    }

    /// Emulate Envoy's buffered request phases without making an upstream call.
    async fn local_request(
        client: &mut ExternalProcessorClient<Channel>,
        method: &str,
        path: &str,
        identity: (&str, &str),
        body: Option<&str>,
    ) -> (i32, String) {
        let (sender, receiver) = tokio::sync::mpsc::channel(1);
        let mut stream = client
            .process(ReceiverStream::new(receiver))
            .await
            .unwrap()
            .into_inner();
        let headers = [
            (":method", method),
            (":path", path),
            (":scheme", "https"),
            (":authority", "test.local"),
            identity,
            ("content-type", "application/json"),
        ]
        .into_iter()
        .map(|(key, value)| HeaderValue {
            key: key.to_owned(),
            value: value.to_owned(),
            raw_value: Vec::new(),
        })
        .collect();
        sender
            .send(ProcessingRequest {
                request: Some(Request::RequestHeaders(HttpHeaders {
                    headers: Some(HeaderMap { headers }),
                    end_of_stream: body.is_none(),
                })),
                ..Default::default()
            })
            .await
            .unwrap();
        let mut response = tokio::time::timeout(Duration::from_secs(5), stream.message())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        if !matches!(response.response, Some(Response::ImmediateResponse(_)))
            && let Some(body) = body
        {
            sender
                .send(ProcessingRequest {
                    request: Some(Request::RequestBody(HttpBody {
                        body: body.as_bytes().to_vec(),
                        end_of_stream: true,
                    })),
                    ..Default::default()
                })
                .await
                .unwrap();
            response = tokio::time::timeout(Duration::from_secs(5), stream.message())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
        }
        assert!(
            matches!(response.response, Some(Response::ImmediateResponse(_))),
            "stored-content must produce a local response: {method} {path}: {response:?}"
        );
        match response.response.unwrap() {
            Response::ImmediateResponse(response) => (response.status.unwrap().code, response.body),
            _ => unreachable!("asserted immediate response"),
        }
    }
}
