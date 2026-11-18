// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Red Hat, Inc.

//! Startup/config validation failures.

use super::support::*;

/// Invalid `GRPC_MAX_RECV_MSG_SIZE` must fail `praxis-extproc -t` (same fail-
/// closed path as serve).
#[test]
fn grpc_max_recv_invalid_fails_validate() {
    let config = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/praxis-extproc.yaml");
    let output = validate_command(&config)
        .env(GRPC_MAX_RECV_MSG_SIZE_ENV, "not-a-size")
        .output()
        .expect("run praxis-extproc -t");
    assert!(
        !output.status.success(),
        "invalid {GRPC_MAX_RECV_MSG_SIZE_ENV} must fail validate, stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains(GRPC_MAX_RECV_MSG_SIZE_ENV) || combined.contains("config:"),
        "validate refusal should name the bad override, got: {combined}"
    );
}


#[test]
fn invalid_listener_addresses_fail_validate() {
    let config = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/praxis-extproc.yaml");

    for flag in ["--grpc-address", "--health-address", "--metrics-address"] {
        let output = validate_command(&config)
            .args([flag, "not-an-address"])
            .output()
            .expect("run praxis-extproc -t");
        let combined = command_output(&output);

        assert!(
            !output.status.success(),
            "invalid {flag} must fail validate, got: {combined}"
        );
        assert!(
            combined.contains("invalid socket address"),
            "validate refusal should identify the invalid address, got: {combined}"
        );
    }
}


#[test]
fn unusable_tls_files_fail_validate() {
    let (_temp_dir, config) = write_invalid_tls_validate_config();
    let output = validate_command(&config).output().expect("run praxis-extproc -t");
    let combined = command_output(&output);

    assert!(
        !output.status.success(),
        "unusable TLS files must fail validate, got: {combined}"
    );
    assert!(
        combined.contains("certificate:"),
        "validate refusal should identify the unusable certificate, got: {combined}"
    );
}

/// Serving must fail fast (non-zero exit) when the gRPC port is already bound,
/// rather than hanging or silently degrading.
#[tokio::test(flavor = "multi_thread")]
async fn serve_fails_when_the_grpc_port_is_already_in_use() {
    let occupied = std::net::TcpListener::bind("127.0.0.1:0").expect("occupy a port");
    let grpc = occupied.local_addr().expect("local addr").port();
    let config = write_grpc_max_recv_config(grpc, free_port(), free_port());

    let mut child = BinaryChild(
        Command::new(env!("CARGO_BIN_EXE_praxis-extproc"))
            .arg("-c")
            .arg(&config)
            .env_remove(GRPC_MAX_RECV_MSG_SIZE_ENV)
            .spawn()
            .expect("spawn praxis-extproc"),
    );

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.0.try_wait().expect("poll child") {
            break Some(status);
        }
        if std::time::Instant::now() > deadline {
            break None;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    };

    drop(std::fs::remove_file(&config));
    drop(occupied);

    if let Some(status) = status {
        assert!(
            !status.success(),
            "serving on an already-bound gRPC port must exit non-zero, got {status:?}"
        );
    } else {
        stop_binary(&mut child).await;
        panic!("server did not exit despite the gRPC port being in use");
    }
}
