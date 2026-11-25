// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Red Hat, Inc.

//! Concurrent streams, header-phase cancellation, and listener startup failures.

use std::{sync::Arc, time::Duration};

use praxis_filter::FilterFactory;
use tokio::sync::{Barrier, Notify};

use super::support::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn simultaneous_streams_on_one_channel_keep_distinct_payloads_and_mutations() {
    let (client, _shutdown) = start_server_with_registry(CHAIN_CONFIG, probe_registry()).await;
    let barrier = Arc::new(Barrier::new(32));
    let requests = (0..32).map(|index| {
        let mut client = client.clone();
        let barrier = Arc::clone(&barrier);
        async move {
            let (tx, mut stream) = open_stream(&mut client).await;
            tx.send(make_request_headers("POST", &format!("/stream/{index}"), false))
                .await
                .unwrap();
            assert!(matches!(
                next(&mut stream).await.response,
                Some(RespVariant::RequestHeaders(_))
            ));
            barrier.wait().await;
            let payload = format!("unique-{index}-café");
            tx.send(request_body(payload.as_bytes(), true)).await.unwrap();
            drop(tx);
            let replies = finish(&mut stream).await;
            assert_eq!(request_bytes(&replies), format!("{payload}:first:second").as_bytes());
            assert_eq!(immediate_status(&replies), None);
        }
    });
    tokio::time::timeout(DEADLINE, futures::future::join_all(requests))
        .await
        .unwrap();
}


#[tokio::test]
async fn cancelled_header_hook_signals_completion_without_release() {
    let gate = HeaderGate {
        started: Arc::new(Notify::new()),
        release: Arc::new(Notify::new()),
        finished: Arc::new(Notify::new()),
    };
    let cfg = serde_yaml::from_str(HEADERS_ONLY_CONFIG).unwrap();
    let pipeline = config::build_pipeline(&cfg, &FilterRegistry::with_builtins()).unwrap();
    let request = praxis_extproc::adapter::envoy_headers_to_request(&[make_header(":path", "/hold")]);
    let mut context = praxis_extproc::adapter::build_filter_context(&pipeline, &request, false);
    let mut hook = Box::pin(gate.on_request(&mut context));

    tokio::time::timeout(DEADLINE, async {
        tokio::select! {
            () = gate.started.notified() => {},
            result = &mut hook => panic!("header hook must wait for release: {result:?}"),
        }
    })
    .await
    .expect("header hook must enter the gate");

    drop(hook);
    tokio::time::timeout(DEADLINE, gate.finished.notified())
        .await
        .expect("dropping the pending hook must signal completion without release");
}

#[tokio::test]
async fn cancellation_while_header_hook_is_pending_allows_the_next_stream() {
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let finished = Arc::new(Notify::new());
    let mut registry = FilterRegistry::with_builtins();
    let gate = HeaderGate {
        started: Arc::clone(&started),
        release: Arc::clone(&release),
        finished: Arc::clone(&finished),
    };
    registry
        .register(
            "first",
            FilterFactory::Http(Arc::new(move |_| Ok(Box::new(gate.clone())))),
        )
        .unwrap();
    let (mut client, _shutdown) = start_server_with_registry(PROBE_CONFIG, registry).await;
    let (tx, stream) = open_stream(&mut client).await;
    tx.send(make_request_headers("GET", "/hold", true)).await.unwrap();
    tokio::time::timeout(DEADLINE, started.notified()).await.unwrap();
    drop(stream);
    drop(tx);
    let replies = tokio::time::timeout(DEADLINE, send_headers_only(&mut client, "GET", "/fresh"))
        .await
        .unwrap();
    assert!(has_request_headers_response(&replies));
    assert_eq!(immediate_status(&replies), None);
    release.notify_one();
    tokio::time::timeout(DEADLINE, finished.notified()).await.unwrap();
}


#[tokio::test]
async fn startup_fails_promptly_when_grpc_port_is_already_bound() {
    let occupied = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let config_path = write_grpc_max_recv_config(occupied.local_addr().unwrap().port(), free_port(), free_port());
    let output_file = tempfile::tempfile().unwrap();
    let mut child = BinaryChild(
        Command::new(env!("CARGO_BIN_EXE_praxis-extproc"))
            .arg("-c")
            .arg(&config_path)
            .stdout(output_file.try_clone().unwrap())
            .stderr(output_file.try_clone().unwrap())
            .spawn()
            .unwrap(),
    );
    let status = tokio::time::timeout(DEADLINE, async {
        loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                break status;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("occupied gRPC port must fail startup without hanging sidecars");
    assert!(!status.success(), "bind failure must be a nonzero exit");
    let mut output = String::new();
    let mut output_file = output_file;
    std::io::Seek::rewind(&mut output_file).unwrap();
    std::io::Read::read_to_string(&mut output_file, &mut output).unwrap();
    assert!(
        output.contains("fatal error=transport error"),
        "startup must fail at the transport bind: {output}"
    );
    std::fs::remove_file(config_path).unwrap();
}
