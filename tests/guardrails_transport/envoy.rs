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
// Serialize allocation through readiness so another world's backend cannot
// claim a port released just before a child process binds it.
static WORLD_STARTUP: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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
        Self::with_modes("BUFFERED", mode, false, reply, hold_json).await
    }

    async fn with_modes(
        request_mode: &str,
        response_mode: &str,
        no_header_wait: bool,
        reply: NemoReply,
        hold_json: bool,
    ) -> Self {
        Self::with_trailers(request_mode, response_mode, no_header_wait, reply, hold_json, "SEND").await
    }

    async fn with_trailers(
        request_mode: &str,
        response_mode: &str,
        no_header_wait: bool,
        reply: NemoReply,
        hold_json: bool,
        request_trailer_mode: &str,
    ) -> Self {
        Self::configured(
            request_mode,
            response_mode,
            no_header_wait,
            reply,
            hold_json,
            request_trailer_mode,
            |_| {},
        )
        .await
    }

    async fn configured(
        request_mode: &str,
        response_mode: &str,
        no_header_wait: bool,
        reply: NemoReply,
        hold_json: bool,
        request_trailer_mode: &str,
        configure: impl FnOnce(&mut praxis_extproc::config::ExtProcConfig),
    ) -> Self {
        let _startup = WORLD_STARTUP.lock().await;
        let envoy = std::env::var_os("ENVOY_BIN").expect("set ENVOY_BIN to the Envoy executable; use make test-envoy");
        let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/envoy-tests")
            .join(format!(
                "{}-{}",
                std::process::id(),
                NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
            ));
        std::fs::create_dir_all(&directory).unwrap();
        // The translator requires selected-upstream body processing, which is
        // independently restricted to BUFFERED requests. FDS worlds exercise
        // guardrails without that incompatible filter.
        let translation = request_mode == "BUFFERED";
        let nemo = Box::pin(Harness::new(reply, translation)).await;
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
        let reserved_grpc = reserve_address();
        let reserved_frontend = reserve_address();
        let reserved_health = reserve_address();
        let reserved_metrics = reserve_address();
        let grpc = reserved_grpc.local_addr().unwrap();
        let frontend = reserved_frontend.local_addr().unwrap();
        let mut cfg = support::configuration(&nemo.endpoint, translation, 2_097_152);
        configure(&mut cfg);
        cfg.server.grpc_address = grpc.to_string();
        cfg.server.health_address = reserved_health.local_addr().unwrap().to_string();
        cfg.server.metrics_address = reserved_metrics.local_addr().unwrap().to_string();
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
        drop((reserved_grpc, reserved_health, reserved_metrics));
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
        std::fs::write(
            &envoy_path,
            envoy_config(
                request_mode,
                response_mode,
                no_header_wait,
                frontend,
                grpc,
                backend,
                request_trailer_mode,
            )
            .to_string(),
        )
        .unwrap();
        let envoy_log = std::fs::File::create(directory.join("envoy.log")).unwrap();
        drop(reserved_frontend);
        let proxy = Process(
            Command::new(envoy)
                .args(["--concurrency", "1", "--disable-hot-restart", "-c"])
                .arg(&envoy_path)
                .stdout(Stdio::from(envoy_log.try_clone().unwrap()))
                .stderr(Stdio::from(envoy_log))
                .spawn()
                .unwrap(),
        );
        let mut world = Self {
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
                    for child in &mut world.children {
                        assert!(
                            child.0.try_wait().unwrap().is_none(),
                            "child exited before readiness; logs: {}",
                            directory.display()
                        );
                    }
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
        self.request(selected, stream).send().await.unwrap()
    }

    fn request(&self, selected: (bool, bool), stream: bool) -> reqwest::RequestBuilder {
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
    }

    fn assert_selection_headers_removed(&self) {
        let requests = self.requests.lock().unwrap().clone();
        assert_eq!(requests.len(), 1);
        assert!(!requests[0].contains_key("x-aigateway-guardrail-input"));
        assert!(!requests[0].contains_key("x-aigateway-guardrail-output"));
    }
}

fn reserve_address() -> std::net::TcpListener {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap()
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

fn envoy_config(
    request_mode: &str,
    response_mode: &str,
    no_header_wait: bool,
    frontend: SocketAddr,
    grpc: SocketAddr,
    backend: SocketAddr,
    request_trailer_mode: &str,
) -> serde_json::Value {
    let address = |port| json!({"socket_address":{"address":"127.0.0.1","port_value":port}});
    let cluster = |name: &str, endpoint: SocketAddr| json!({"name":name,"type":"STATIC","connect_timeout":"2s","load_assignment":{"cluster_name":name,"endpoints":[{"lb_endpoints":[{"endpoint":{"address":address(endpoint.port())}}]}]}});
    let mut processor = cluster("extproc", grpc);
    processor["http2_protocol_options"] = json!({});
    json!({"static_resources":{
        "listeners":[{"name":"http","address":address(frontend.port()),"filter_chains":[{"filters":[{"name":"envoy.filters.network.http_connection_manager","typed_config":{
            "@type":"type.googleapis.com/envoy.extensions.filters.network.http_connection_manager.v3.HttpConnectionManager","stat_prefix":"test","http_protocol_options":{"enable_trailers":true},"route_config":{"name":"test","virtual_hosts":[{"name":"all","domains":["*"],"routes":[{"match":{"prefix":"/"},"route":{"cluster":"backend","timeout":"0s"}}]}]},
            "http_filters":[{"name":"envoy.filters.http.ext_proc","typed_config":{"@type":"type.googleapis.com/envoy.extensions.filters.http.ext_proc.v3.ExternalProcessor","grpc_service":{"envoy_grpc":{"cluster_name":"extproc"},"timeout":"10s"},"failure_mode_allow":false,"allow_mode_override":true,"message_timeout":"10s","send_body_without_waiting_for_header_response":no_header_wait,"processing_mode":{"request_header_mode":"SEND","response_header_mode":"SEND","request_body_mode":request_mode,"response_body_mode":response_mode,"request_trailer_mode":request_trailer_mode,"response_trailer_mode":"SEND"}}},{"name":"envoy.filters.http.router","typed_config":{"@type":"type.googleapis.com/envoy.extensions.filters.http.router.v3.Router"}}]
        }}]}]}],"clusters":[processor,cluster("backend", backend)]}})
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn envoy_fds_withholds_all_output_until_the_complete_guardrail_verdict() {
    for no_header_wait in [false, true] {
        for reply in [NemoReply::BlockOutput, NemoReply::Unavailable, NemoReply::Timeout] {
            let world = World::with_modes(
                "FULL_DUPLEX_STREAMED",
                "FULL_DUPLEX_STREAMED",
                no_header_wait,
                reply,
                true,
            )
            .await;
            let request = world.request((false, true), false);
            let (first_bytes, mut bytes_seen) = tokio::sync::mpsc::channel(1);
            let client = tokio::spawn(async move {
                let response = request.send().await.unwrap();
                let status = response.status();
                let mut stream = response.bytes_stream();
                let mut output = Vec::new();
                while let Some(chunk) = stream.next().await {
                    let chunk = chunk.unwrap();
                    if !chunk.is_empty() {
                        let _sent = first_bytes.try_send(());
                        output.extend_from_slice(&chunk);
                    }
                }
                (status, output)
            });
            tokio::time::timeout(DEADLINE, world.first_frame.notified())
                .await
                .expect("backend must emit its first frame before checking privacy");
            assert!(
                tokio::time::timeout(Duration::from_millis(150), bytes_seen.recv())
                    .await
                    .is_err(),
                "FDS must release no client bytes before the complete verdict"
            );
            assert!(
                world.nemo.calls().is_empty(),
                "FDS must not submit a partial document to NeMo"
            );
            world.finish.notify_one();
            let (status, output) = tokio::time::timeout(DEADLINE, client)
                .await
                .expect("replacement must terminate at EOS")
                .unwrap();
            assert_eq!(status, 200);
            let body: serde_json::Value =
                serde_json::from_slice(&output).expect("one complete replacement document with no leaked prefix");
            let code = if matches!(reply, NemoReply::BlockOutput) {
                "content_blocked"
            } else {
                "evaluation_failed"
            };
            assert_eq!(body["error"]["code"], code, "no_header_wait={no_header_wait}");
            assert_eq!(body.as_object().unwrap().len(), 1);
            assert!(!String::from_utf8_lossy(&output).contains("private answer"));
            let calls = world.nemo.calls();
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0]["guardrails"]["rail_types"][0], "output");
            assert_eq!(calls[0]["messages"][0]["content"], "private answer");
            world.assert_selection_headers_removed();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn envoy_fds_input_verdict_controls_backend_dispatch() {
    for no_header_wait in [false, true] {
        for reply in [
            NemoReply::Pass,
            NemoReply::BlockInput,
            NemoReply::Unavailable,
            NemoReply::Timeout,
        ] {
            let world = World::with_modes(
                "FULL_DUPLEX_STREAMED",
                "FULL_DUPLEX_STREAMED",
                no_header_wait,
                reply,
                false,
            )
            .await;
            let response = world.send((true, false), false).await;
            let expected = match reply {
                NemoReply::Pass => 200,
                NemoReply::BlockInput => 403,
                _ => 500,
            };
            assert_eq!(
                response.status(),
                expected,
                "{reply:?}, no_header_wait={no_header_wait}"
            );
            let body = response.text().await.unwrap();
            if matches!(reply, NemoReply::Pass) {
                assert!(body.contains("private answer"));
                world.assert_selection_headers_removed();
            } else {
                assert!(
                    world.requests.lock().unwrap().is_empty(),
                    "a rejected prompt must never reach the model"
                );
                assert!(!body.contains("private answer"));
            }
            let calls = world.nemo.calls();
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0]["guardrails"]["rail_types"][0], "input");
            assert_eq!(calls[0]["messages"][0]["content"], "hello");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn envoy_rejects_unchecked_request_modes_before_backend_dispatch() {
    for request_mode in ["NONE", "STREAMED"] {
        for no_header_wait in [false, true] {
            let world = World::with_modes(
                request_mode,
                "FULL_DUPLEX_STREAMED",
                no_header_wait,
                NemoReply::Pass,
                false,
            )
            .await;
            let response = world.send((false, true), false).await;
            assert_eq!(response.status(), 500);
            assert!(
                response
                    .text()
                    .await
                    .unwrap()
                    .contains("require BUFFERED or FULL_DUPLEX_STREAMED request bodies")
            );
            assert!(world.requests.lock().unwrap().is_empty());
            assert!(world.nemo.calls().is_empty());
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn envoy_fds_releases_sse_before_backend_completion() {
    for no_header_wait in [false, true] {
        for input in [false, true] {
            let world = World::with_modes(
                "FULL_DUPLEX_STREAMED",
                "FULL_DUPLEX_STREAMED",
                no_header_wait,
                NemoReply::Pass,
                false,
            )
            .await;
            let response = world.send((input, false), true).await;
            assert_eq!(response.status(), 200);
            let mut stream = response.bytes_stream();
            let first = tokio::time::timeout(DEADLINE, stream.next())
                .await
                .expect("FDS first token must arrive while backend completion is blocked")
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
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn envoy_checks_buffered_requests_with_skipped_http_trailers() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    for response_mode in ["BUFFERED", "STREAMED", "FULL_DUPLEX_STREAMED"] {
        let world = World::with_trailers("BUFFERED", response_mode, false, NemoReply::Pass, false, "SKIP").await;
        let uri: http::Uri = world.url.parse().unwrap();
        let mut socket = tokio::net::TcpStream::connect(uri.authority().unwrap().as_str())
            .await
            .unwrap();
        let body = json!({"model":"test","messages":[{"role":"user","content":"trailer prompt"}]}).to_string();
        let message = format!(
            "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nTrailer: x-test-trailer\r\nConnection: close\r\nx-aigateway-guardrail-input: true\r\nx-aigateway-guardrail-output: true\r\n\r\n{:x}\r\n{}\r\n0\r\nx-test-trailer: finished\r\n\r\n",
            uri.path(),
            uri.authority().unwrap(),
            body.len(),
            body,
        );
        socket.write_all(message.as_bytes()).await.unwrap();
        let mut bytes = Vec::new();
        tokio::time::timeout(DEADLINE, socket.read_to_end(&mut bytes))
            .await
            .unwrap()
            .unwrap();
        let response = String::from_utf8(bytes).unwrap();
        assert!(response.starts_with("HTTP/1.1 200"), "{response_mode}: {response}");
        assert!(response.contains("private answer"), "{response}");
        let calls = world.nemo.calls();
        assert_eq!(calls.len(), 2, "input and output must each run once");
        assert_eq!(calls[0]["messages"][0]["content"], "trailer prompt");
        assert_eq!(calls[1]["messages"][0]["content"], "private answer");
        assert_eq!(world.requests.lock().unwrap().len(), 1);
        world.assert_selection_headers_removed();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn envoy_preserves_appended_request_headers_and_response_cookies() {
    for request_mode in ["BUFFERED", "FULL_DUPLEX_STREAMED"] {
        for response_mode in ["BUFFERED", "STREAMED", "FULL_DUPLEX_STREAMED"] {
            let world = World::configured(request_mode, response_mode, false, NemoReply::Pass, false, "SEND", |cfg| {
            cfg.filter_chains[0].filters.push(serde_yaml::from_str(
                "filter: headers\nrequest_add:\n  - {name: x-values, value: first}\n  - {name: x-values, value: second}\nresponse_add:\n  - {name: set-cookie, value: session=first}\n  - {name: set-cookie, value: theme=second}\n"
            ).unwrap());
        }).await;
            let response = world.send((false, false), false).await;
            assert_eq!(response.status(), 200);
            let cookies: Vec<_> = response
                .headers()
                .get_all("set-cookie")
                .iter()
                .map(|value| value.to_str().unwrap())
                .collect();
            assert_eq!(
                cookies,
                ["session=first", "theme=second"],
                "{response_mode}: Envoy must append both cookies"
            );
            assert!(response.text().await.unwrap().contains("private answer"));
            let requests = world.requests.lock().unwrap();
            assert_eq!(requests.len(), 1);
            let values: Vec<_> = requests[0]
                .get_all("x-values")
                .iter()
                .flat_map(|value| value.to_str().unwrap().split(',').map(str::trim))
                .collect();
            assert_eq!(values, ["first", "second"], "request additions must append on the wire");
            assert!(world.nemo.calls().is_empty());
        }
    }
}
