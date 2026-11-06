// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Red Hat, Inc.

//! Gated SSE backend for Kubernetes transport qualification.

use std::{convert::Infallible, sync::Arc};

use bytes::Bytes;
use futures::StreamExt as _;
use http_body_util::{BodyExt as _, Full, StreamBody, combinators::UnsyncBoxBody};
use hyper::{
    Request, Response,
    body::{Frame, Incoming},
    service::service_fn,
};
use tokio::{net::TcpListener, sync::Notify};

/// Serve one gated SSE exchange at a time on port 8080.
///
/// `/oracle/started` waits for the first frame, `/oracle/release` releases the
/// final event, and `/ready` is a readiness probe.
/// The fixture is only compiled into the Kubernetes qualification image.
///
/// # Errors
///
/// Returns an error if the listener cannot bind or accept a connection.
pub async fn serve() -> std::io::Result<()> {
    let listener = TcpListener::bind("0.0.0.0:8080").await?;
    let gate = Arc::new(StreamGate::default());
    loop {
        let (socket, _) = listener.accept().await?;
        let gate = Arc::clone(&gate);
        drop(tokio::spawn(async move {
            let service = service_fn(move |request| respond(request, Arc::clone(&gate)));
            if let Err(error) = hyper::server::conn::http1::Builder::new()
                .serve_connection(hyper_util::rt::TokioIo::new(socket), service)
                .await
            {
                tracing::debug!(%error, "SSE fixture connection closed");
            }
        }));
    }
}

/// Synchronize backend frame delivery with the qualification client.
#[derive(Default)]
struct StreamGate {
    /// Permit delivery of the final event.
    release: Notify,

    /// Announce that the first frame has been yielded to the HTTP server.
    started: Notify,
}

/// Respond to control requests or hold the stream open until release.
async fn respond(
    request: Request<Incoming>,
    gate: Arc<StreamGate>,
) -> Result<Response<UnsyncBoxBody<Bytes, Infallible>>, Infallible> {
    let path = request.uri().path();
    let body = match path {
        "/ready" => Full::new(Bytes::from_static(b"ready")).boxed_unsync(),
        "/oracle/started" => {
            gate.started.notified().await;
            Full::new(Bytes::from_static(b"started")).boxed_unsync()
        },
        "/oracle/release" => {
            gate.release.notify_one();
            Full::new(Bytes::from_static(b"released")).boxed_unsync()
        },
        _ => return Ok(stream_response(request, gate).await),
    };
    Ok(Response::new(body))
}

/// Produce an SSE response after consuming the client's request.
async fn stream_response(
    request: Request<Incoming>,
    gate: Arc<StreamGate>,
) -> Response<UnsyncBoxBody<Bytes, Infallible>> {
    if let Err(error) = request.into_body().collect().await {
        tracing::debug!(%error, "SSE fixture request body failed");
    }
    let mut response = Response::new(gated_body(gate));
    response.headers_mut().insert(
        hyper::header::CONTENT_TYPE,
        hyper::header::HeaderValue::from_static("text/event-stream"),
    );
    response
}

/// Emit the terminal SSE event only after the test has received the first one.
fn gated_body(gate: Arc<StreamGate>) -> UnsyncBoxBody<Bytes, Infallible> {
    let first = futures::stream::once(async { Ok(Frame::data(Bytes::from_static(b"data: first\n\n"))) });
    let last = futures::stream::once(async move {
        gate.started.notify_one();
        gate.release.notified().await;
        Ok(Frame::data(Bytes::from_static(b"data: [DONE]\n\n")))
    });
    StreamBody::new(first.chain(last)).boxed_unsync()
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use futures::FutureExt as _;

    use super::*;

    #[tokio::test]
    async fn final_event_waits_for_release() {
        let gate = Arc::new(StreamGate::default());
        let mut body = gated_body(Arc::clone(&gate));
        assert!(
            gate.started.notified().now_or_never().is_none(),
            "backend has not yielded a frame yet"
        );
        let first = body.frame().await.unwrap().unwrap().into_data().unwrap();
        assert_eq!(first, "data: first\n\n", "first event must be available immediately");
        assert!(body.frame().now_or_never().is_none(), "EOS must wait for the client");
        assert!(
            gate.started.notified().now_or_never().is_some(),
            "first frame must be announced"
        );
        gate.release.notify_one();
        let last = body.frame().await.unwrap().unwrap().into_data().unwrap();
        assert_eq!(last, "data: [DONE]\n\n", "release must deliver the terminal event");
        assert!(body.frame().await.is_none(), "the released stream must end");
    }
}
