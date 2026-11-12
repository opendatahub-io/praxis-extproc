//! Late rejection follows `ProcessingResponse.immediate_response` in the
//! vendored `proto/proto/ext_proc.proto`: earlier output cannot be retracted.

use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
};

use bytes::Bytes;
use praxis_extproc::{
    config::{ExtProcConfig, build_pipeline},
    server::PraxisExtProc,
};
use praxis_filter::{
    BodyAccess, FilterAction, FilterError, FilterFactory, FilterRegistry, HttpFilter, HttpFilterContext, Rejection,
};
use praxis_proto::envoy::service::ext_proc::v3::{
    BodySendMode, ProtocolConfiguration, external_processor_client::ExternalProcessorClient,
    external_processor_server::ExternalProcessorServer, processing_response::Response as Reply,
};
use tokio::{net::TcpListener, sync::mpsc, task::JoinHandle};
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tonic::transport::Server;

use super::support::{self, Session, body_bytes, immediate_status, response_body, response_headers};

pub(super) const FIRST: &[u8] = b"data: first\n\n";

#[tokio::test]
async fn immediate_response_ends_stream_after_first_or_later_chunk_rejection() {
    for mode in [BodySendMode::Streamed, BodySendMode::FullDuplexStreamed] {
        for accepted_chunks in [0, 1, 3] {
            for reject_at_eos in [false, true] {
                let server = RejectingServer::new(accepted_chunks * FIRST.len()).await;
                let mut session = server.session(mode).await;
                for index in 0..accepted_chunks {
                    session.send(response_body(FIRST, false)).await;
                    if mode == BodySendMode::FullDuplexStreamed && index == 0 {
                        assert!(matches!(
                            session.next().await.unwrap().response,
                            Some(Reply::ResponseHeaders(_))
                        ));
                    }
                    let accepted = session.next().await.unwrap();
                    assert_eq!(
                        body_bytes(&[accepted]),
                        FIRST,
                        "accepted chunks must be emitted before rejection"
                    );
                }

                session
                    .sender
                    .try_send(response_body(b"reject", reject_at_eos))
                    .unwrap();
                session.sender.try_send(response_body(b"later", true)).unwrap();
                let rejected = session.next().await.unwrap();
                assert_eq!(immediate_status(std::slice::from_ref(&rejected)), Some(403));
                assert!(body_bytes(&[rejected]).is_empty());
                assert!(
                    session.next().await.is_none(),
                    "no queued body or trailer reply may follow rejection"
                );
                let calls = server.calls();
                assert_eq!(
                    calls.len(),
                    accepted_chunks + 1,
                    "queued later chunks must not reach filters"
                );
                assert_eq!(calls.last().unwrap(), b"reject");
            }
        }
    }
}

// -----------------------------------------------------------------------------
// Test Utilities
// -----------------------------------------------------------------------------

pub(super) struct RejectingServer {
    pub(super) address: SocketAddr,
    calls: Arc<Mutex<Vec<Vec<u8>>>>,
    task: JoinHandle<()>,
}

impl Drop for RejectingServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl RejectingServer {
    pub(super) async fn new(accepted_bytes: usize) -> Self {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let record = Arc::clone(&calls);
        let mut registry = FilterRegistry::with_builtins();
        registry
            .register(
                "late_rejection",
                FilterFactory::Http(Arc::new(move |_| {
                    Ok(Box::new(LateRejection {
                        accepted_bytes,
                        calls: Arc::clone(&record),
                    }))
                })),
            )
            .unwrap();
        let cfg: ExtProcConfig =
            serde_yaml::from_str("filter_chains:\n  - name: main\n    filters:\n      - filter: late_rejection\n")
                .unwrap();
        let service = ExternalProcessorServer::new(PraxisExtProc::new(build_pipeline(&cfg, &registry).unwrap()));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            Server::builder()
                .add_service(service)
                .serve_with_incoming(TcpListenerStream::new(listener))
                .await
                .unwrap();
        });
        Self { address, calls, task }
    }

    pub(super) fn calls(&self) -> Vec<Vec<u8>> {
        self.calls.lock().unwrap().clone()
    }

    async fn session(&self, mode: BodySendMode) -> Session {
        let mut client = Box::pin(ExternalProcessorClient::connect(format!("http://{}", self.address)))
            .await
            .unwrap();
        let (sender, receiver) = mpsc::channel(8);
        let responses = client
            .process(ReceiverStream::new(receiver))
            .await
            .unwrap()
            .into_inner();
        let mut session = Session { sender, responses };
        let protocol = ProtocolConfiguration {
            request_body_mode: BodySendMode::None.into(),
            response_body_mode: mode.into(),
            ..Default::default()
        };
        let headers = session
            .exchange(support::request_headers(protocol, (false, false), true))
            .await;
        assert!(matches!(headers.response, Some(Reply::RequestHeaders(_))));
        session.send(response_headers("text/event-stream")).await;
        if mode != BodySendMode::FullDuplexStreamed {
            assert!(matches!(
                session.next().await.unwrap().response,
                Some(Reply::ResponseHeaders(_))
            ));
        }
        session
    }
}

struct LateRejection {
    accepted_bytes: usize,
    calls: Arc<Mutex<Vec<Vec<u8>>>>,
}

#[async_trait::async_trait]
impl HttpFilter for LateRejection {
    fn name(&self) -> &'static str {
        "late_rejection"
    }

    async fn on_request(&self, _: &mut HttpFilterContext<'_>) -> Result<FilterAction, FilterError> {
        Ok(FilterAction::Continue)
    }

    fn response_body_access(&self) -> BodyAccess {
        BodyAccess::ReadOnly
    }

    fn response_body_mode(&self) -> praxis_filter::BodyMode {
        praxis_filter::BodyMode::Stream
    }

    fn on_response_body(
        &self,
        _: &mut HttpFilterContext<'_>,
        body: &mut Option<Bytes>,
        _: bool,
    ) -> Result<FilterAction, FilterError> {
        let bytes = body.as_deref().unwrap_or_default();
        if bytes.is_empty() {
            return Ok(FilterAction::Continue);
        }
        let mut calls = self.calls.lock().unwrap();
        let total = calls.iter().map(Vec::len).sum::<usize>() + bytes.len();
        calls.push(bytes.to_vec());
        drop(calls);
        Ok(if total > self.accepted_bytes {
            FilterAction::Reject(Rejection::status(403))
        } else {
            FilterAction::Continue
        })
    }
}
