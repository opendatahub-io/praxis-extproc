//! HTTP contracts through real Envoy and the compiled product executable.
//!
//! These use the ExtProc protocol documented in Envoy's
//! `service/ext_proc/v3/external_processor.proto`: an absent body mutation
//! forwards original bytes, whereas an empty mutation suppresses them.

use std::{
    convert::Infallible,
    net::SocketAddr,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use bytes::Bytes;
use futures::StreamExt as _;
use http_body_util::{BodyExt as _, Full, StreamBody, combinators::UnsyncBoxBody};
use hyper::{
    Request, Response,
    body::{Frame, Incoming},
    service::service_fn,
};
use tokio::{net::TcpListener, sync::Notify, task::JoinHandle};

use super::*;

const DEADLINE: Duration = Duration::from_secs(10);
static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn envoy_releases_sse_before_the_backend_is_allowed_to_finish() {
    for input in [false, true] {
        let world = World::new("STREAMED", NemoReply::Pass, false).await;
        let response = world.send((input, false), true).await;
        assert_eq!(response.status(), 200);
        let mut stream = response.bytes_stream();
        let first = tokio::time::timeout(DEADLINE, stream.next())
            .await
            .expect("first token must arrive while backend completion is blocked")
            .unwrap()
            .unwrap();
        assert!(String::from_utf8_lossy(&first).contains("first"));
        assert_eq!(world.nemo.calls().len(), usize::from(input));
        world.finish.notify_one();
        let mut rest = Vec::new();
        while let Some(chunk) = tokio::time::timeout(DEADLINE, stream.next()).await.unwrap() {
            rest.extend_from_slice(&chunk.unwrap());
        }
        assert!(String::from_utf8_lossy(&rest).contains("[DONE]"));
        world.assert_selection_headers_removed();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn envoy_checks_both_phases_in_buffered_and_streamed_configurations() {
    for mode in ["BUFFERED", "STREAMED"] {
        let world = World::new(mode, NemoReply::BlockOutput, false).await;
        let response = world.send((true, true), false).await;
        assert_eq!(response.status(), 200);
        let body = response.text().await.unwrap();
        assert!(body.contains("content_blocked"), "{mode}: {body}");
        assert!(!body.contains("private answer"));
        assert_eq!(
            world.nemo.calls().len(),
            2,
            "Input and Output must both run through the deployed process"
        );
        world.assert_selection_headers_removed();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn envoy_never_releases_partial_output_before_the_guardrail_verdict() {
    let world = World::new("STREAMED", NemoReply::BlockOutput, true).await;
    let response = world.send((false, true), false).await;
    let mut stream = response.bytes_stream();
    tokio::time::timeout(DEADLINE, world.first_frame.notified())
        .await
        .expect("backend must yield its first frame before checking for leaks");
    assert!(
        tokio::time::timeout(Duration::from_millis(150), stream.next())
            .await
            .is_err(),
        "partial output must be withheld"
    );
    assert!(
        world.nemo.calls().is_empty(),
        "no verdict may be requested on incomplete JSON"
    );
    world.finish.notify_one();
    let mut output = Vec::new();
    while let Some(chunk) = tokio::time::timeout(DEADLINE, stream.next()).await.unwrap() {
        output.extend_from_slice(&chunk.unwrap());
    }
    let body: serde_json::Value = serde_json::from_slice(&output)
        .expect("the response must be one complete error document without a leaked JSON prefix");
    assert_eq!(body["error"]["code"], "content_blocked");
    assert_eq!(body["error"]["type"], "guardrail_violation");
    assert_eq!(
        body.as_object().unwrap().len(),
        1,
        "only the guardrail error may be sent"
    );
    assert_eq!(world.nemo.calls().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn envoy_rejects_output_guarded_sse_before_contacting_the_model() {
    let world = World::new("STREAMED", NemoReply::Pass, false).await;
    let response = world.send((false, true), true).await;
    assert_eq!(response.status(), 422);
    assert!(
        response
            .text()
            .await
            .unwrap()
            .contains("streaming responses are not supported")
    );
    assert!(
        world.requests.lock().unwrap().is_empty(),
        "the model must not be called on rejection"
    );
    assert!(world.nemo.calls().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn envoy_preserves_non_streaming_anthropic_translation() {
    let world = World::new("STREAMED", NemoReply::Pass, true).await;
    let url = world.url.replace("/v1/chat/completions", "/v1/messages");
    let response = reqwest::Client::builder()
        .timeout(DEADLINE)
        .no_proxy()
        .build()
        .unwrap()
        .post(url)
        .json(&json!({"model":"test","max_tokens":16,"messages":[{"role":"user","content":"hello"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    world.finish.notify_one();
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(
        body["type"], "message",
        "translator must still receive the complete JSON: {body}"
    );
    assert_eq!(body["content"][0]["text"], "private answer");
    assert!(world.nemo.calls().is_empty());
}

struct Process(Child);

impl Drop for Process {
    fn drop(&mut self) {
        drop(self.0.kill());
        drop(self.0.wait());
    }
}

struct World {
    children: Vec<Process>,
    finish: Arc<Notify>,
    first_frame: Arc<Notify>,
    nemo: Harness,
    requests: Arc<Mutex<Vec<http::HeaderMap>>>,
    task: JoinHandle<()>,
    url: String,
}

impl Drop for World {
    fn drop(&mut self) {
        self.children.clear();
        self.task.abort();
    }
}

impl World {
    async fn new(mode: &str, reply: NemoReply, hold_json: bool) -> Self {
        let envoy = std::env::var_os("ENVOY_BIN").expect("set ENVOY_BIN to the Envoy executable; use make test-envoy");
        let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/envoy-tests")
            .join(format!(
                "{}-{}",
                std::process::id(),
                NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
            ));
        std::fs::create_dir_all(&directory).unwrap();
        let nemo = Box::pin(Harness::new(reply, true)).await;
        let finish = Arc::new(Notify::new());
        let first_frame = Arc::new(Notify::new());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend = listener.local_addr().unwrap();
        let task = spawn_backend(
            listener,
            Arc::clone(&finish),
            Arc::clone(&first_frame),
            Arc::clone(&requests),
            hold_json,
        );
        let grpc = unused_address();
        let frontend = unused_address();
        let mut cfg = support::configuration(&nemo.endpoint, true, 2_097_152);
        cfg.server.grpc_address = grpc.to_string();
        cfg.server.health_address = unused_address().to_string();
        cfg.server.metrics_address = unused_address().to_string();
        let config_path = directory.join("extproc.yaml");
        // metrics_auth defaults to enabled and exits when this process has no kubeconfig.
        let config = serde_yaml::to_value(serde_json::json!({
            "filter_chains": cfg.filter_chains,
            "insecure_options": {"allow_private_upstreams": true},
            "server": {
                "grpc_address": cfg.server.grpc_address,
                "health_address": cfg.server.health_address,
                "metrics_address": cfg.server.metrics_address,
                "max_body_bytes": 2_097_152,
                "metrics_auth": {"enabled": false},
            }
        }))
        .unwrap();
        std::fs::write(&config_path, serde_yaml::to_string(&config).unwrap()).unwrap();
        let extproc_log = std::fs::File::create(directory.join("extproc.log")).unwrap();
        let extproc = Process(
            Command::new(env!("CARGO_BIN_EXE_praxis-extproc"))
                .arg("-c")
                .arg(&config_path)
                .stdout(Stdio::from(extproc_log.try_clone().unwrap()))
                .stderr(Stdio::from(extproc_log))
                .spawn()
                .unwrap(),
        );
        let envoy_path = directory.join("envoy.json");
        std::fs::write(&envoy_path, envoy_config(mode, frontend, grpc, backend).to_string()).unwrap();
        let envoy_log = std::fs::File::create(directory.join("envoy.log")).unwrap();
        let proxy = Process(
            Command::new(envoy)
                .args(["--concurrency", "1", "--disable-hot-restart", "-c"])
                .arg(&envoy_path)
                .stdout(Stdio::from(envoy_log.try_clone().unwrap()))
                .stderr(Stdio::from(envoy_log))
                .spawn()
                .unwrap(),
        );
        let world = Self {
            children: vec![extproc, proxy],
            finish,
            first_frame,
            nemo,
            requests,
            task,
            url: format!("http://{frontend}/v1/chat/completions"),
        };
        for address in [grpc, frontend] {
            tokio::time::timeout(DEADLINE, async {
                loop {
                    if tokio::net::TcpStream::connect(address).await.is_ok() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap_or_else(|error| panic!("listener {address} failed: {error}; logs: {}", directory.display()));
        }
        world
    }

    async fn send(&self, selected: (bool, bool), stream: bool) -> reqwest::Response {
        reqwest::Client::builder()
            .timeout(DEADLINE)
            .no_proxy()
            .build()
            .unwrap()
            .post(&self.url)
            .header("x-aigateway-guardrail-input", if selected.0 { "true" } else { "false" })
            .header(
                "x-aigateway-guardrail-output",
                if selected.1 { "true" } else { "false" },
            )
            .json(&json!({"model":"test","messages":[{"role":"user","content":"hello"}],"stream":stream}))
            .send()
            .await
            .unwrap()
    }

    fn assert_selection_headers_removed(&self) {
        let requests = self.requests.lock().unwrap().clone();
        assert_eq!(requests.len(), 1);
        assert!(!requests[0].contains_key("x-aigateway-guardrail-input"));
        assert!(!requests[0].contains_key("x-aigateway-guardrail-output"));
    }
}

fn unused_address() -> SocketAddr {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}

fn spawn_backend(
    listener: TcpListener,
    finish: Arc<Notify>,
    first_frame: Arc<Notify>,
    requests: Arc<Mutex<Vec<http::HeaderMap>>>,
    hold_json: bool,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut connections = tokio::task::JoinSet::new();
        while let Ok((socket, _)) = listener.accept().await {
            let finish = Arc::clone(&finish);
            let first_frame = Arc::clone(&first_frame);
            let requests = Arc::clone(&requests);
            connections.spawn(async move {
                let service = service_fn(move |request| {
                    backend_response(
                        request,
                        Arc::clone(&finish),
                        Arc::clone(&first_frame),
                        Arc::clone(&requests),
                        hold_json,
                    )
                });
                drop(
                    hyper::server::conn::http1::Builder::new()
                        .serve_connection(hyper_util::rt::TokioIo::new(socket), service)
                        .await,
                );
            });
        }
    })
}

async fn backend_response(
    request: Request<Incoming>,
    finish: Arc<Notify>,
    first_frame: Arc<Notify>,
    requests: Arc<Mutex<Vec<http::HeaderMap>>>,
    hold_json: bool,
) -> Result<Response<UnsyncBoxBody<Bytes, Infallible>>, Infallible> {
    requests.lock().unwrap().push(request.headers().clone());
    let body = request.into_body().collect().await.unwrap().to_bytes();
    let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let streaming = value["stream"] == true;
    let json = serde_json::to_vec(&json!({
        "id":"chatcmpl-test", "object":"chat.completion", "created":1, "model":"test",
        "choices":[{"index":0,"message":{"role":"assistant","content":"private answer"},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":1,"completion_tokens":2,"total_tokens":3}
    }))
    .unwrap();
    let body = if streaming || hold_json {
        let (first, last) = if streaming {
            (
                Bytes::from_static(b"data: first\n\n"),
                Bytes::from_static(b"data: [DONE]\n\n"),
            )
        } else {
            let split = json.len() / 2;
            (
                Bytes::copy_from_slice(&json[..split]),
                Bytes::copy_from_slice(&json[split..]),
            )
        };
        let chunks = futures::stream::unfold(
            (0, first, last, finish, first_frame),
            |(step, first, last, finish, first_frame)| async move {
                let chunk = match step {
                    0 => first.clone(),
                    1 => {
                        first_frame.notify_one();
                        finish.notified().await;
                        last.clone()
                    },
                    _ => return None,
                };
                Some((
                    Ok::<_, Infallible>(Frame::data(chunk)),
                    (step + 1, first, last, finish, first_frame),
                ))
            },
        );
        StreamBody::new(chunks).boxed_unsync()
    } else {
        Full::new(Bytes::from(json)).boxed_unsync()
    };
    Ok(Response::builder()
        .header(
            "content-type",
            if streaming {
                "text/event-stream"
            } else {
                "application/json"
            },
        )
        .body(body)
        .unwrap())
}

fn envoy_config(mode: &str, frontend: SocketAddr, grpc: SocketAddr, backend: SocketAddr) -> serde_json::Value {
    let address = |port| json!({"socket_address":{"address":"127.0.0.1","port_value":port}});
    let cluster = |name: &str, endpoint: SocketAddr| json!({"name":name,"type":"STATIC","connect_timeout":"2s","load_assignment":{"cluster_name":name,"endpoints":[{"lb_endpoints":[{"endpoint":{"address":address(endpoint.port())}}]}]}});
    let mut processor = cluster("extproc", grpc);
    processor["http2_protocol_options"] = json!({});
    json!({"static_resources":{
        "listeners":[{"name":"http","address":address(frontend.port()),"filter_chains":[{"filters":[{"name":"envoy.filters.network.http_connection_manager","typed_config":{
            "@type":"type.googleapis.com/envoy.extensions.filters.network.http_connection_manager.v3.HttpConnectionManager","stat_prefix":"test","route_config":{"name":"test","virtual_hosts":[{"name":"all","domains":["*"],"routes":[{"match":{"prefix":"/"},"route":{"cluster":"backend","timeout":"0s"}}]}]},
            "http_filters":[{"name":"envoy.filters.http.ext_proc","typed_config":{"@type":"type.googleapis.com/envoy.extensions.filters.http.ext_proc.v3.ExternalProcessor","grpc_service":{"envoy_grpc":{"cluster_name":"extproc"},"timeout":"10s"},"failure_mode_allow":false,"allow_mode_override":true,"message_timeout":"10s","processing_mode":{"request_header_mode":"SEND","response_header_mode":"SEND","request_body_mode":"BUFFERED","response_body_mode":mode}}},{"name":"envoy.filters.http.router","typed_config":{"@type":"type.googleapis.com/envoy.extensions.filters.http.router.v3.Router"}}]
        }}]}]}],"clusters":[processor,cluster("backend", backend)]}})
}
