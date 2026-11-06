//! Fail-closed behavior when the BBR / pre-processing ExtProc is unavailable.

use std::{
    process::Command,
    sync::Mutex,
    time::{Duration, Instant},
};

use crate::fixtures::{chat_completion, ensure_gateway_ready};

/// Serialize cluster mutations that take pre-processing down (shared gateway).
static PRE_PROC_LOCK: Mutex<()> = Mutex::new(());

const PRE_PROC_DEPLOY: &str = "payload-pre-processing";
const PRE_PROC_SVC: &str = "payload-pre-processing";
const PRE_PROC_NS: &str = "istio-system";

fn kubectl_context() -> String {
    std::env::var("E2E_CONTEXT").unwrap_or_else(|_| "kind-praxis-e2e".to_owned())
}

fn kubectl(args: &[&str]) -> std::process::Output {
    let mut cmd = Command::new("kubectl");
    cmd.args(["--context", &kubectl_context()]);
    cmd.args(args);
    cmd.output().unwrap_or_else(|e| panic!("kubectl {args:?}: {e}"))
}

fn kubectl_stdout(args: &[&str]) -> String {
    String::from_utf8_lossy(&kubectl(args).stdout).trim().to_owned()
}

fn parse_replicas(text: &str) -> u32 {
    if text.is_empty() {
        0
    } else {
        text.parse::<u32>().unwrap_or(0)
    }
}

fn scale_pre_processing(replicas: u32) {
    let out = kubectl(&[
        "-n",
        PRE_PROC_NS,
        "scale",
        &format!("deploy/{PRE_PROC_DEPLOY}"),
        &format!("--replicas={replicas}"),
    ]);
    assert!(
        out.status.success(),
        "scale {PRE_PROC_DEPLOY} to {replicas}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Wait until the Deployment reports the desired ready replica count.
fn wait_pre_processing_ready(want: u32, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        let text = kubectl_stdout(&[
            "-n",
            PRE_PROC_NS,
            "get",
            &format!("deploy/{PRE_PROC_DEPLOY}"),
            "-o",
            "jsonpath={.status.availableReplicas}",
        ]);
        let have = parse_replicas(&text);
        if have == want {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {PRE_PROC_DEPLOY} availableReplicas={want} (have {have})"
        );
        std::thread::sleep(Duration::from_secs(2));
    }
}

/// Wait until scaled-to-zero is fully drained.
///
/// Deployment `status.replicas` / Endpoints can clear while a terminating pod
/// is still alive and answering Envoy's pooled HTTP/2 ExtProc connection
/// (manual repro: empty replicas+endpoints with `pods=1` still returns 200;
/// once `pods=0`, Envoy returns 500). Wait for zero pods, not just status.
fn wait_pre_processing_gone(timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        let replicas = parse_replicas(&kubectl_stdout(&[
            "-n",
            PRE_PROC_NS,
            "get",
            &format!("deploy/{PRE_PROC_DEPLOY}"),
            "-o",
            "jsonpath={.status.replicas}",
        ]));
        let endpoints = kubectl_stdout(&[
            "-n",
            PRE_PROC_NS,
            "get",
            &format!("endpoints/{PRE_PROC_SVC}"),
            "-o",
            "jsonpath={.subsets[*].addresses[*].ip}",
        ]);
        // `--no-headers` yields one line per pod (including Terminating).
        let pod_lines = kubectl_stdout(&[
            "-n",
            PRE_PROC_NS,
            "get",
            "pods",
            "-l",
            &format!("app={PRE_PROC_DEPLOY}"),
            "--no-headers",
        ]);
        let pods = if pod_lines.is_empty() {
            0_usize
        } else {
            pod_lines.lines().filter(|l| !l.is_empty()).count()
        };
        if replicas == 0 && endpoints.is_empty() && pods == 0 {
            // Give Envoy a beat to drop pooled connections to the old pods.
            std::thread::sleep(Duration::from_secs(2));
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {PRE_PROC_DEPLOY} drained \
             (status.replicas={replicas}, endpoints={endpoints:?}, pods={pods})"
        );
        std::thread::sleep(Duration::from_secs(2));
    }
}

/// When BBR ExtProc has no endpoints, Envoy must reject (`failure_mode_allow:
/// false`), not route with a caller-controlled or missing model header.
#[tokio::test]
async fn unavailable_pre_processor_fails_closed() {
    let _guard = PRE_PROC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    ensure_gateway_ready().await;

    scale_pre_processing(0);
    wait_pre_processing_gone(Duration::from_secs(90));

    let status = {
        let resp = chat_completion("gpt-4", "hello").await;
        let status = resp.status();
        // Drain body so the connection is reusable / errors are visible.
        drop(resp.bytes().await);
        status
    };

    scale_pre_processing(1);
    wait_pre_processing_ready(1, Duration::from_secs(120));

    assert!(
        status.is_server_error(),
        "unavailable pre-processor must fail closed (5xx), got {status}"
    );

    // Gateway must recover once BBR is back.
    let recovered = chat_completion("gpt-4", "hello").await;
    assert_eq!(
        recovered.status(),
        200,
        "gpt-4 must succeed again after pre-processor recovery"
    );
}
