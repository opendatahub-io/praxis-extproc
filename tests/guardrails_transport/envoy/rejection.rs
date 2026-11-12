//! `ProcessingResponse.immediate_response` after downstream output has started.

use super::*;
use crate::stream_rejection::{FIRST, RejectingServer};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn envoy_immediate_response_after_committed_chunk_resets_without_rejected_bytes() {
    for mode in ["STREAMED", "FULL_DUPLEX_STREAMED"] {
        for no_header_wait in [false, true] {
            let world = RejectionWorld::new(mode, no_header_wait).await;
            let response = reqwest::Client::builder()
                .timeout(DEADLINE)
                .no_proxy()
                .build()
                .unwrap()
                .post(&world.url)
                .json(&json!({"stream":true}))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 200);
            let mut stream = response.bytes_stream();
            let mut received = Vec::new();
            while received.len() < FIRST.len() {
                received.extend_from_slice(
                    &tokio::time::timeout(DEADLINE, stream.next())
                        .await
                        .unwrap()
                        .unwrap()
                        .unwrap(),
                );
            }
            assert_eq!(
                received, FIRST,
                "first output must reach the client before rejection is possible"
            );
            world.finish.notify_one();

            let terminal = tokio::time::timeout(DEADLINE, stream.next()).await.unwrap();
            let error = terminal
                .expect("Envoy must terminate the incomplete response with an error")
                .expect_err("Envoy 1.37 resets the committed HTTP/1 response");
            assert!(
                !error.is_timeout(),
                "a client deadline must not count as a reset: {error}"
            );
            assert!(
                error.is_body() || error.is_decode(),
                "expected a body-stream error: {error}"
            );
            assert!(tokio::time::timeout(DEADLINE, stream.next()).await.unwrap().is_none());
            assert!(
                world.processor.calls().iter().map(Vec::len).sum::<usize>() > FIRST.len(),
                "the filter must see and reject the later upstream bytes"
            );
        }
    }
}

// -----------------------------------------------------------------------------
// Test Utilities
// -----------------------------------------------------------------------------

struct RejectionWorld {
    proxy: Process,
    backend: JoinHandle<()>,
    finish: Arc<Notify>,
    processor: RejectingServer,
    url: String,
}

impl Drop for RejectionWorld {
    fn drop(&mut self) {
        self.backend.abort();
    }
}

impl RejectionWorld {
    async fn new(mode: &str, no_header_wait: bool) -> Self {
        let _startup = WORLD_STARTUP.lock().await;
        let processor = RejectingServer::new(FIRST.len()).await;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend_address = listener.local_addr().unwrap();
        let finish = Arc::new(Notify::new());
        let backend = spawn_backend(
            listener,
            Arc::clone(&finish),
            Arc::new(Notify::new()),
            Arc::new(Mutex::new(Vec::new())),
            false,
        );
        let reserved_frontend = reserve_address();
        let frontend = reserved_frontend.local_addr().unwrap();
        let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/envoy-tests")
            .join(format!(
                "{}-{}-late-rejection",
                std::process::id(),
                NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
            ));
        std::fs::create_dir_all(&directory).unwrap();
        let config = directory.join("envoy.json");
        std::fs::write(
            &config,
            envoy_config(
                EnvoyModes::new("NONE", mode, no_header_wait),
                frontend,
                processor.address,
                backend_address,
            )
            .to_string(),
        )
        .unwrap();
        let log = std::fs::File::create(directory.join("envoy.log")).unwrap();
        drop(reserved_frontend);
        let proxy = Process(
            Command::new(std::env::var_os("ENVOY_BIN").expect("use make test-envoy"))
                .args(["--concurrency", "1", "--disable-hot-restart", "-c"])
                .arg(config)
                .stdout(Stdio::from(log.try_clone().unwrap()))
                .stderr(Stdio::from(log))
                .spawn()
                .unwrap(),
        );
        let mut world = Self {
            proxy,
            backend,
            finish,
            processor,
            url: format!("http://{frontend}/v1/chat/completions"),
        };
        tokio::time::timeout(DEADLINE, async {
            loop {
                assert!(
                    world.proxy.0.try_wait().unwrap().is_none(),
                    "Envoy exited; logs: {}",
                    directory.display()
                );
                if tokio::net::TcpStream::connect(frontend).await.is_ok() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        world
    }
}
