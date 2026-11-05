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
/// `/oracle/release` releases the final event; `/ready` is a readiness probe.
/// The fixture is only compiled into the Kubernetes qualification image.
///
/// # Errors
///
/// Returns an error if the listener cannot bind or accept a connection.
pub async fn serve() -> std::io::Result<()> {
    let listener = TcpListener::bind("0.0.0.0:8080").await?;
    let release = Arc::new(Notify::new());
    loop {
        let (socket, _) = listener.accept().await?;
        let release = Arc::clone(&release);
        drop(tokio::spawn(async move {
            let service = service_fn(move |request| respond(request, Arc::clone(&release)));
            if let Err(error) = hyper::server::conn::http1::Builder::new()
                .serve_connection(hyper_util::rt::TokioIo::new(socket), service)
                .await
            {
                tracing::debug!(%error, "SSE fixture connection closed");
            }
        }));
    }
}

/// Respond to control requests or hold the stream open until release.
async fn respond(
    request: Request<Incoming>,
    release: Arc<Notify>,
) -> Result<Response<UnsyncBoxBody<Bytes, Infallible>>, Infallible> {
    let path = request.uri().path();
    let body = match path {
        "/ready" => Full::new(Bytes::from_static(b"ready")).boxed_unsync(),
        "/oracle/release" => {
            release.notify_one();
            Full::new(Bytes::from_static(b"released")).boxed_unsync()
        },
        _ => {
            if let Err(error) = request.into_body().collect().await {
                tracing::debug!(%error, "SSE fixture request body failed");
            }
            let mut response = Response::new(gated_body(release));
            response.headers_mut().insert(
                hyper::header::CONTENT_TYPE,
                hyper::header::HeaderValue::from_static("text/event-stream"),
            );
            return Ok(response);
        },
    };
    Ok(Response::new(body))
}

/// Emit the terminal SSE event only after the test has received the first one.
fn gated_body(release: Arc<Notify>) -> UnsyncBoxBody<Bytes, Infallible> {
    let first = futures::stream::once(async { Ok(Frame::data(Bytes::from_static(b"data: first\n\n"))) });
    let last = futures::stream::once(async move {
        release.notified().await;
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
        let release = Arc::new(Notify::new());
        let mut body = gated_body(Arc::clone(&release));
        let first = body.frame().await.unwrap().unwrap().into_data().unwrap();
        assert_eq!(first, "data: first\n\n", "first event must be available immediately");
        assert!(body.frame().now_or_never().is_none(), "EOS must wait for the client");
        release.notify_one();
        let last = body.frame().await.unwrap().unwrap().into_data().unwrap();
        assert_eq!(last, "data: [DONE]\n\n", "release must deliver the terminal event");
        assert!(body.frame().await.is_none(), "the released stream must end");
    }
}
