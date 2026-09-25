// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Red Hat, Inc.

//! ExtProc protocol sequencing and configuration.
//!
//! Parses Envoy's per-stream [`ProtocolConfiguration`] into a [`ProtocolConfig`]
//! and enforces the ordering rules of the ExtProc message exchange: per-direction
//! phase progression ([`PhaseOrderTracker`]), end-of-stream lifecycle
//! ([`EosTracker`]), and rejection of body messages for phases Envoy configured
//! not to send.

use praxis_proto::envoy::service::ext_proc::v3::{ProtocolConfiguration, processing_request};
use tonic::Status;

use crate::{metrics, response::BodyMode};

// -----------------------------------------------------------------------------
// Envoy Protocol Configuration
// -----------------------------------------------------------------------------

/// Parsed protocol configuration from Envoy.
///
/// Extracted from the first `ProcessingRequest` message's `protocol_config` field.
#[derive(Debug, Clone, Default)]
pub(crate) struct ProtocolConfig {
    /// Request body processing mode.
    pub(crate) request_body_mode: BodyMode,

    /// Response body processing mode.
    pub(crate) response_body_mode: BodyMode,

    /// Whether body is sent immediately without waiting for header response.
    /// Only applies to `STREAMED` body mode per Envoy spec; ignored for other
    /// modes. `FULL_DUPLEX_STREAMED` inherently streams body without waiting.
    ///
    /// See: `ProtocolConfiguration.send_body_without_waiting_for_header_response`
    #[expect(dead_code, reason = "captured for future STREAMED delayed-response implementation")]
    send_body_without_waiting: bool,
}

impl TryFrom<ProtocolConfiguration> for ProtocolConfig {
    type Error = String;

    fn try_from(proto_cfg: ProtocolConfiguration) -> Result<Self, Self::Error> {
        Ok(Self {
            request_body_mode: BodyMode::try_from(proto_cfg.request_body_mode)
                .map_err(|e| format!("request_body_mode: {e}"))?,
            response_body_mode: BodyMode::try_from(proto_cfg.response_body_mode)
                .map_err(|e| format!("response_body_mode: {e}"))?,
            send_body_without_waiting: proto_cfg.send_body_without_waiting_for_header_response,
        })
    }
}

// -----------------------------------------------------------------------------
// Phase Ordering
// -----------------------------------------------------------------------------

/// Which direction of the exchange a message belongs to.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum PhaseSide {
    /// Request-side phases (headers, body, trailers).
    Request,

    /// Response-side phases (headers, body, trailers).
    Response,
}

/// Ordered position within one direction's phase sequence.
///
/// The derived ordering is `Headers` < `Body` < `Trailers`.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum PhaseStep {
    /// Headers phase.
    Headers,

    /// Body phase.
    Body,

    /// Trailers phase.
    Trailers,
}

/// Direction and step of a message within its direction's sequence.
///
/// Each direction advances monotonically (headers → body → trailers), but the
/// two directions are independent: in `FULL_DUPLEX_STREAMED` Envoy interleaves
/// request-body chunks with response processing, so ordering is enforced per
/// direction rather than globally.
const fn message_order(req: &processing_request::Request) -> (PhaseSide, PhaseStep) {
    match req {
        processing_request::Request::RequestHeaders(_) => (PhaseSide::Request, PhaseStep::Headers),
        processing_request::Request::RequestBody(_) => (PhaseSide::Request, PhaseStep::Body),
        processing_request::Request::RequestTrailers(_) => (PhaseSide::Request, PhaseStep::Trailers),
        processing_request::Request::ResponseHeaders(_) => (PhaseSide::Response, PhaseStep::Headers),
        processing_request::Request::ResponseBody(_) => (PhaseSide::Response, PhaseStep::Body),
        processing_request::Request::ResponseTrailers(_) => (PhaseSide::Response, PhaseStep::Trailers),
    }
}

/// Tracks per-direction phase progression to reject out-of-order messages.
///
/// Each direction advances monotonically (`Headers` → `Body` → `Trailers`); the
/// two are independent so `FULL_DUPLEX_STREAMED` interleaving is allowed.
/// A stream that opens with a response message carries a local reply from an
/// earlier filter (an auth 401, a rate-limit 429): Envoy runs it through this
/// filter's encoder path only, so no request message may follow. The ExtProc
/// API allows a stream without request headers (`request_header_mode: SKIP`),
/// which looks the same and is treated the same way.
///
/// See: `envoy/extensions/filters/http/ext_proc/v3/processing_mode.proto`
/// (`ProcessingMode.HeaderSendMode`) and Envoy's
/// `DownstreamFilterManager::sendLocalReply` (`source/common/http/filter_manager.cc`).
#[derive(Debug, Default)]
pub(crate) struct PhaseOrderTracker {
    /// Furthest request-side step seen.
    request: Option<PhaseStep>,

    /// Furthest response-side step seen.
    response: Option<PhaseStep>,

    /// Whether the stream opened with a response message.
    local_reply: bool,
}

impl PhaseOrderTracker {
    /// Validate a message's position and advance the tracker.
    ///
    /// Equal steps are allowed (repeated body chunks); duplicate-EOS is caught
    /// per-phase by [`EosTracker`]. Called before any handler mutates state, so a
    /// rejection leaves no partial per-stream state.
    ///
    /// # Errors
    ///
    /// Returns [`Status::invalid_argument`] when `req` regresses within its
    /// direction, or when a request message follows a local reply.
    pub(crate) fn check_and_advance(&mut self, req: &processing_request::Request) -> Result<(), Status> {
        let (side, step) = message_order(req);
        if side == PhaseSide::Request && self.local_reply {
            metrics::record_invalid_argument("message_order", "request_after_local_reply");
            return Err(Status::invalid_argument(format!(
                "out-of-order ExtProc message: {} arrived after a local reply",
                request_type_label(req)
            )));
        }
        // Request phases always open with headers, so an empty request side
        // means this filter never saw the request.
        let opens_local_reply = side == PhaseSide::Response && self.request.is_none();
        let current = match side {
            PhaseSide::Request => &mut self.request,
            PhaseSide::Response => &mut self.response,
        };
        let invalid_transition = match *current {
            None => step != PhaseStep::Headers,
            Some(prev) => step < prev || (step == prev && step != PhaseStep::Body),
        };

        if invalid_transition {
            metrics::record_invalid_argument("message_order", "invalid_phase_transition");
            return Err(Status::invalid_argument(format!(
                "out-of-order ExtProc message: invalid {side:?} phase transition to {}",
                request_type_label(req)
            )));
        }
        *current = Some(step);
        self.local_reply |= opens_local_reply;
        Ok(())
    }

    /// Whether the stream opened with a local reply from an earlier filter.
    pub(crate) const fn local_reply(&self) -> bool {
        self.local_reply
    }
}

/// Reject body messages for phases Envoy configured not to send.
///
/// This check intentionally runs before phase ordering, EOS tracking, body
/// accumulation, and filter execution. A body message in `NONE` mode is a
/// malformed ExtProc stream, not an empty body to process.
pub(crate) fn validate_body_message(req: &processing_request::Request, config: &ProtocolConfig) -> Result<(), Status> {
    let (phase, mode) = match req {
        processing_request::Request::RequestBody(_) => ("RequestBody", config.request_body_mode),
        processing_request::Request::ResponseBody(_) => ("ResponseBody", config.response_body_mode),
        _ => return Ok(()),
    };

    if mode == BodyMode::None {
        metrics::record_invalid_argument("body_mode", "body_message_in_none_mode");
        return Err(Status::invalid_argument(format!(
            "received {phase} message while its body mode is NONE"
        )));
    }

    Ok(())
}

// -----------------------------------------------------------------------------
// EOS Tracking
// -----------------------------------------------------------------------------

/// Protocol phase identifier for EOS tracking.
#[derive(Debug, Copy, Clone)]
pub(crate) enum ProtocolPhase {
    /// Request headers phase.
    RequestHeaders,

    /// Request body phase.
    RequestBody,

    /// Response headers phase.
    ResponseHeaders,

    /// Response body phase.
    ResponseBody,
}

/// End-of-stream lifecycle state of a single protocol phase.
///
/// Doubles as the outcome of [`EosTracker::check_and_mark`], which returns the
/// phase's state *on entry*: [`PhaseState::Completed`] means `end_of_stream` was
/// already seen, so the current message is a re-delivery.
#[derive(Debug, Default, Copy, Clone, PartialEq, Eq)]
pub(crate) enum PhaseState {
    /// No `end_of_stream` seen yet; the phase is still being processed.
    #[default]
    Active,

    /// `end_of_stream` received; the phase is complete.
    Completed,
}

impl PhaseState {
    /// Whether the phase has completed (`end_of_stream` seen).
    pub(crate) const fn is_complete(self) -> bool {
        matches!(self, Self::Completed)
    }
}

/// Tracks end-of-stream status for each protocol phase.
#[derive(Debug, Default)]
pub(crate) struct EosTracker {
    /// Request headers phase state.
    pub(crate) request_headers: PhaseState,

    /// Request body phase state.
    pub(crate) request_body: PhaseState,

    /// Response headers phase state.
    pub(crate) response_headers: PhaseState,

    /// Response body phase state.
    pub(crate) response_body: PhaseState,
}

impl EosTracker {
    /// Current state of a phase.
    fn phase_state(&self, phase: ProtocolPhase) -> PhaseState {
        match phase {
            ProtocolPhase::RequestHeaders => self.request_headers,
            ProtocolPhase::RequestBody => self.request_body,
            ProtocolPhase::ResponseHeaders => self.response_headers,
            ProtocolPhase::ResponseBody => self.response_body,
        }
    }

    /// Detect re-delivery and mark end-of-stream for a protocol phase.
    ///
    /// Returns the phase's state *on entry*: [`PhaseState::Completed`] means the
    /// message is a re-delivery, leaving the benign-vs-violation policy to the
    /// caller (which knows the body mode). A message on a body phase whose headers
    /// phase already ended is always a genuine sequencing violation, rejected here.
    ///
    /// # Errors
    ///
    /// Returns [`Status::invalid_argument`] if a body message arrives after its
    /// headers phase has ended.
    pub(crate) fn check_and_mark(&mut self, phase: ProtocolPhase, received_eos: bool) -> Result<PhaseState, Status> {
        // An already-completed phase means this message is a re-delivery; the
        // caller decides whether that is benign (FDS) or a violation.
        if self.phase_state(phase).is_complete() {
            return Ok(PhaseState::Completed);
        }

        // For body phases: a message after the corresponding headers phase ended
        // is a genuine sequencing violation regardless of mode.
        let headers_completed = match phase {
            ProtocolPhase::RequestBody => self.request_headers.is_complete(),
            ProtocolPhase::ResponseBody => self.response_headers.is_complete(),
            ProtocolPhase::RequestHeaders | ProtocolPhase::ResponseHeaders => false,
        };

        if headers_completed {
            metrics::record_invalid_argument("message_order", "body_after_headers_eos");
            return Err(Status::invalid_argument(format!(
                "received {phase:?} message after headers end_of_stream was already marked"
            )));
        }

        if received_eos {
            match phase {
                ProtocolPhase::RequestHeaders => self.request_headers = PhaseState::Completed,
                ProtocolPhase::RequestBody => self.request_body = PhaseState::Completed,
                ProtocolPhase::ResponseHeaders => self.response_headers = PhaseState::Completed,
                ProtocolPhase::ResponseBody => self.response_body = PhaseState::Completed,
            }
        }

        Ok(PhaseState::Active)
    }
}

/// Error for a message re-delivered after its phase already completed.
pub(crate) fn duplicate_after_eos(phase: ProtocolPhase) -> Status {
    metrics::record_invalid_argument("duplicate_eos", "redelivery");
    Status::invalid_argument(format!(
        "received {phase:?} message after end_of_stream was already marked"
    ))
}

// -----------------------------------------------------------------------------
// Utilities
// -----------------------------------------------------------------------------

/// Label string for a request variant, used in debug logging.
pub(crate) fn request_type_label(req: &processing_request::Request) -> &'static str {
    match req {
        processing_request::Request::RequestHeaders(_) => "request_headers",
        processing_request::Request::RequestBody(_) => "request_body",
        processing_request::Request::ResponseHeaders(_) => "response_headers",
        processing_request::Request::ResponseBody(_) => "response_body",
        processing_request::Request::RequestTrailers(_) => "request_trailers",
        processing_request::Request::ResponseTrailers(_) => "response_trailers",
    }
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;
    use crate::test_support::invalid_arg_count;

    #[test]
    fn phase_state_default_is_active() {
        let state = PhaseState::default();
        assert_eq!(state, PhaseState::Active, "default phase state should be Active");
        assert!(!state.is_complete(), "default phase state should not be complete");
    }

    #[test]
    fn eos_tracker_default_all_active() {
        let tracker = EosTracker::default();
        assert!(
            !tracker.request_headers.is_complete(),
            "request_headers should be Active"
        );
        assert!(!tracker.request_body.is_complete(), "request_body should be Active");
        assert!(
            !tracker.response_headers.is_complete(),
            "response_headers should be Active"
        );
        assert!(!tracker.response_body.is_complete(), "response_body should be Active");
    }

    #[test]
    fn eos_tracker_first_eos_succeeds() {
        let mut tracker = EosTracker::default();
        assert!(
            tracker.check_and_mark(ProtocolPhase::RequestHeaders, true).is_ok(),
            "first EOS in RequestHeaders should succeed"
        );

        let mut tracker = EosTracker::default();
        assert!(
            tracker.check_and_mark(ProtocolPhase::RequestBody, true).is_ok(),
            "first EOS in RequestBody should succeed"
        );

        let mut tracker = EosTracker::default();
        assert!(
            tracker.check_and_mark(ProtocolPhase::ResponseHeaders, true).is_ok(),
            "first EOS in ResponseHeaders should succeed"
        );

        let mut tracker = EosTracker::default();
        assert!(
            tracker.check_and_mark(ProtocolPhase::ResponseBody, true).is_ok(),
            "first EOS in ResponseBody should succeed"
        );
    }

    #[test]
    fn eos_tracker_duplicate_eos_reports_duplicate() {
        let mut tracker = EosTracker::default();

        // First EOS is a fresh message to process.
        assert_eq!(
            tracker.check_and_mark(ProtocolPhase::RequestHeaders, true).unwrap(),
            PhaseState::Active
        );

        // Re-delivery is reported as Completed (policy is left to the caller).
        assert_eq!(
            tracker.check_and_mark(ProtocolPhase::RequestHeaders, true).unwrap(),
            PhaseState::Completed,
            "re-delivery should report Completed"
        );
    }

    #[test]
    fn eos_tracker_duplicate_eos_in_each_phase_reports_duplicate() {
        // Test re-delivery detection in each phase independently.
        // Use separate trackers since body phases are blocked after header EOS.
        let phases = [
            ProtocolPhase::RequestHeaders,
            ProtocolPhase::RequestBody,
            ProtocolPhase::ResponseHeaders,
            ProtocolPhase::ResponseBody,
        ];

        for phase in phases {
            let mut tracker = EosTracker::default();

            assert_eq!(
                tracker.check_and_mark(phase, true).unwrap(),
                PhaseState::Active,
                "first EOS should be Active for {phase:?}"
            );

            assert_eq!(
                tracker.check_and_mark(phase, true).unwrap(),
                PhaseState::Completed,
                "re-delivery should report Completed for {phase:?}"
            );
        }
    }

    /// Assert every message classifies to `side` with strictly increasing steps.
    fn assert_monotonic(side: PhaseSide, msgs: &[processing_request::Request]) {
        let orders = msgs.iter().map(message_order).collect::<Vec<_>>();
        assert!(
            orders.iter().all(|(s, _)| *s == side),
            "messages must classify as {side:?}, got {orders:?}"
        );
        assert!(
            orders
                .windows(2)
                .all(|w| w.first().map(|t| t.1) < w.last().map(|t| t.1)),
            "steps must be strictly increasing, got {orders:?}"
        );
    }

    #[test]
    fn message_order_is_monotonic_within_each_direction() {
        use praxis_proto::envoy::service::ext_proc::v3::{HttpBody, HttpHeaders, HttpTrailers};
        use processing_request::Request;

        assert_monotonic(
            PhaseSide::Request,
            &[
                Request::RequestHeaders(HttpHeaders::default()),
                Request::RequestBody(HttpBody::default()),
                Request::RequestTrailers(HttpTrailers::default()),
            ],
        );
        assert_monotonic(
            PhaseSide::Response,
            &[
                Request::ResponseHeaders(HttpHeaders::default()),
                Request::ResponseBody(HttpBody::default()),
                Request::ResponseTrailers(HttpTrailers::default()),
            ],
        );
    }

    #[test]
    fn phase_order_allows_forward_and_repeated_phases() {
        use praxis_proto::envoy::service::ext_proc::v3::{HttpBody, HttpHeaders};
        use processing_request::Request;
        let mut tracker = PhaseOrderTracker::default();

        for req in [
            Request::RequestHeaders(HttpHeaders::default()),
            Request::RequestBody(HttpBody::default()),
            Request::RequestBody(HttpBody::default()),
            Request::ResponseHeaders(HttpHeaders::default()),
            Request::ResponseBody(HttpBody::default()),
        ] {
            assert!(
                tracker.check_and_advance(&req).is_ok(),
                "forward/repeated sequence must be accepted, rejected at {req:?}"
            );
        }
    }

    #[test]
    fn phase_order_allows_full_duplex_interleaving() {
        use praxis_proto::envoy::service::ext_proc::v3::{HttpBody, HttpHeaders};
        use processing_request::Request;
        let mut tracker = PhaseOrderTracker::default();

        // FULL_DUPLEX_STREAMED: request-body chunks may interleave with response
        // processing. RequestHeaders -> RequestBody -> ResponseHeaders -> RequestBody
        // is a legal sequence and must not be rejected.
        for req in [
            Request::RequestHeaders(HttpHeaders::default()),
            Request::RequestBody(HttpBody::default()),
            Request::ResponseHeaders(HttpHeaders::default()),
            Request::RequestBody(HttpBody::default()),
        ] {
            assert!(
                tracker.check_and_advance(&req).is_ok(),
                "interleaved sequence must be accepted, rejected at {req:?}"
            );
        }
    }

    #[test]
    fn phase_order_rejects_within_direction_regression() {
        use praxis_proto::envoy::service::ext_proc::v3::{HttpBody, HttpHeaders};
        use processing_request::Request;
        let mut tracker = PhaseOrderTracker::default();

        for req in [
            Request::RequestHeaders(HttpHeaders::default()),
            Request::ResponseHeaders(HttpHeaders::default()),
            Request::ResponseBody(HttpBody::default()),
        ] {
            assert!(tracker.check_and_advance(&req).is_ok());
        }

        // ResponseHeaders after ResponseBody regresses within the response direction.
        let result = tracker.check_and_advance(&Request::ResponseHeaders(HttpHeaders::default()));
        assert!(result.is_err(), "ResponseHeaders after ResponseBody should be rejected");
        if let Err(err) = result {
            assert_eq!(err.code(), tonic::Code::InvalidArgument);
            assert!(err.message().contains("out-of-order"));
        }
    }

    #[test]
    fn phase_order_accepts_local_reply_and_rejects_request_after_it() {
        use praxis_proto::envoy::service::ext_proc::v3::{HttpBody, HttpHeaders, HttpTrailers};
        use processing_request::Request;
        let mut tracker = PhaseOrderTracker::default();

        for req in [
            Request::ResponseHeaders(HttpHeaders::default()),
            Request::ResponseBody(HttpBody::default()),
            Request::ResponseTrailers(HttpTrailers::default()),
        ] {
            assert!(
                tracker.check_and_advance(&req).is_ok(),
                "local reply phases must be accepted"
            );
        }
        assert!(tracker.local_reply, "a response-first stream is a local reply");

        let result = tracker.check_and_advance(&Request::RequestHeaders(HttpHeaders::default()));
        assert!(
            result.is_err(),
            "request headers after a local reply should be rejected"
        );
        if let Err(err) = result {
            assert_eq!(err.code(), tonic::Code::InvalidArgument);
            assert!(err.message().contains("after a local reply"));
        }
    }

    #[test]
    fn phase_order_invalid_first_response_is_not_a_local_reply() {
        use praxis_proto::envoy::service::ext_proc::v3::HttpBody;
        use processing_request::Request;
        let mut tracker = PhaseOrderTracker::default();

        let result = tracker.check_and_advance(&Request::ResponseBody(HttpBody::default()));

        assert!(result.is_err(), "a stream cannot open with a response body");
        assert!(
            !tracker.local_reply,
            "a rejected message must leave the tracker unchanged"
        );
    }

    #[test]
    fn phase_order_request_first_stream_is_not_a_local_reply() {
        use praxis_proto::envoy::service::ext_proc::v3::HttpHeaders;
        use processing_request::Request;
        let mut tracker = PhaseOrderTracker::default();

        for req in [
            Request::RequestHeaders(HttpHeaders::default()),
            Request::ResponseHeaders(HttpHeaders::default()),
        ] {
            assert!(tracker.check_and_advance(&req).is_ok());
        }
        assert!(
            !tracker.local_reply,
            "a stream that saw request headers is not a local reply"
        );
    }

    #[test]
    fn phase_order_rejects_request_direction_regression() {
        use praxis_proto::envoy::service::ext_proc::v3::{HttpBody, HttpHeaders, HttpTrailers};
        use processing_request::Request;
        let mut tracker = PhaseOrderTracker::default();

        assert!(
            tracker
                .check_and_advance(&Request::RequestHeaders(HttpHeaders::default()))
                .is_ok()
        );
        assert!(
            tracker
                .check_and_advance(&Request::RequestTrailers(HttpTrailers::default()))
                .is_ok()
        );

        let result = tracker.check_and_advance(&Request::RequestBody(HttpBody::default()));
        assert!(result.is_err(), "RequestBody after RequestTrailers should be rejected");
        assert_eq!(result.unwrap_err().code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn eos_tracker_false_eos_is_noop() {
        let mut tracker = EosTracker::default();

        // Calling with received_eos=false should be a no-op
        assert!(tracker.check_and_mark(ProtocolPhase::RequestHeaders, false).is_ok());
        assert!(!tracker.request_headers.is_complete(), "phase should stay Active");

        // Can still mark it later
        assert!(tracker.check_and_mark(ProtocolPhase::RequestHeaders, true).is_ok());
        assert!(tracker.request_headers.is_complete(), "phase should now be Completed");
    }

    #[test]
    fn eos_tracker_body_blocked_after_headers() {
        let mut tracker = EosTracker::default();

        assert!(tracker.check_and_mark(ProtocolPhase::RequestHeaders, true).is_ok());

        let result = tracker.check_and_mark(ProtocolPhase::RequestBody, true);
        assert!(
            result.is_err(),
            "RequestBody should be blocked after RequestHeaders EOS"
        );
        if let Err(err) = result {
            assert_eq!(err.code(), tonic::Code::InvalidArgument);
            assert!(err.message().contains("after headers end_of_stream"));
        }

        assert!(tracker.check_and_mark(ProtocolPhase::ResponseHeaders, true).is_ok());

        let result = tracker.check_and_mark(ProtocolPhase::ResponseBody, true);
        assert!(
            result.is_err(),
            "ResponseBody should be blocked after ResponseHeaders EOS"
        );
        if let Err(err) = result {
            assert_eq!(err.code(), tonic::Code::InvalidArgument);
            assert!(err.message().contains("after headers end_of_stream"));
        }
    }

    #[test]
    fn eos_tracker_multiple_false_then_true() {
        let mut tracker = EosTracker::default();

        // Multiple false calls should all be no-ops
        for _ in 0..5 {
            assert!(tracker.check_and_mark(ProtocolPhase::RequestBody, false).is_ok());
            assert!(!tracker.request_body.is_complete());
        }

        // First true should succeed
        assert!(tracker.check_and_mark(ProtocolPhase::RequestBody, true).is_ok());
        assert!(tracker.request_body.is_complete());

        // Subsequent message (even with false) is a re-delivery.
        assert_eq!(
            tracker.check_and_mark(ProtocolPhase::RequestBody, false).unwrap(),
            PhaseState::Completed,
            "re-delivery should report Completed even with end_of_stream=false"
        );

        // Subsequent true is also a re-delivery.
        assert_eq!(
            tracker.check_and_mark(ProtocolPhase::RequestBody, true).unwrap(),
            PhaseState::Completed,
            "re-delivery should report Completed"
        );
    }

    #[test]
    fn duplicate_after_eos_error_includes_phase() {
        let test_cases = [
            (ProtocolPhase::RequestHeaders, "RequestHeaders"),
            (ProtocolPhase::RequestBody, "RequestBody"),
            (ProtocolPhase::ResponseHeaders, "ResponseHeaders"),
            (ProtocolPhase::ResponseBody, "ResponseBody"),
        ];

        for (phase, expected_name) in test_cases {
            let err = duplicate_after_eos(phase);
            assert_eq!(err.code(), tonic::Code::InvalidArgument);
            assert!(
                err.message().contains("after end_of_stream"),
                "error for {phase:?} should mention 'after end_of_stream', got: {}",
                err.message()
            );
            assert!(
                err.message().contains(expected_name),
                "error for {phase:?} should contain '{expected_name}', got: {}",
                err.message()
            );
        }
    }

    #[test]
    fn eos_tracker_reports_duplicate_regardless_of_flag() {
        let mut tracker = EosTracker::default();

        // Mark EOS
        assert!(tracker.check_and_mark(ProtocolPhase::RequestBody, true).is_ok());

        // A re-delivery is reported as Completed whatever the end_of_stream flag.
        assert_eq!(
            tracker.check_and_mark(ProtocolPhase::RequestBody, false).unwrap(),
            PhaseState::Completed
        );
        assert_eq!(
            tracker.check_and_mark(ProtocolPhase::RequestBody, true).unwrap(),
            PhaseState::Completed
        );
    }

    #[test]
    fn phase_order_request_after_local_reply_records_metric() {
        use praxis_proto::envoy::service::ext_proc::v3::HttpHeaders;
        use processing_request::Request;

        let mut tracker = PhaseOrderTracker::default();
        assert!(
            tracker
                .check_and_advance(&Request::ResponseHeaders(HttpHeaders::default()))
                .is_ok(),
            "local reply must be accepted"
        );
        let count = invalid_arg_count("message_order", "request_after_local_reply", || {
            assert!(
                tracker
                    .check_and_advance(&Request::RequestHeaders(HttpHeaders::default()))
                    .is_err(),
                "request after a local reply must be rejected"
            );
        });
        assert_eq!(count, 1, "request-after-local-reply must increment the counter");
    }

    #[test]
    fn phase_order_invalid_transition_records_metric() {
        use praxis_proto::envoy::service::ext_proc::v3::{HttpBody, HttpHeaders, HttpTrailers};
        use processing_request::Request;

        let mut tracker = PhaseOrderTracker::default();
        assert!(
            tracker
                .check_and_advance(&Request::RequestHeaders(HttpHeaders::default()))
                .is_ok()
        );
        assert!(
            tracker
                .check_and_advance(&Request::RequestTrailers(HttpTrailers::default()))
                .is_ok()
        );

        let count = invalid_arg_count("message_order", "invalid_phase_transition", || {
            assert!(
                tracker
                    .check_and_advance(&Request::RequestBody(HttpBody::default()))
                    .is_err(),
                "RequestBody after RequestTrailers must be rejected"
            );
        });
        assert_eq!(count, 1, "invalid phase transition must increment the counter");
    }

    #[test]
    fn eos_body_after_headers_records_metric() {
        let mut tracker = EosTracker::default();
        assert!(tracker.check_and_mark(ProtocolPhase::RequestHeaders, true).is_ok());

        let count = invalid_arg_count("message_order", "body_after_headers_eos", || {
            assert!(
                tracker.check_and_mark(ProtocolPhase::RequestBody, true).is_err(),
                "body after headers EOS must be rejected"
            );
        });
        assert_eq!(count, 1, "body-after-headers-eos must increment the counter");
    }

    #[test]
    fn duplicate_after_eos_records_metric() {
        let count = invalid_arg_count("duplicate_eos", "redelivery", || {
            let err = duplicate_after_eos(ProtocolPhase::RequestBody);
            assert_eq!(err.code(), tonic::Code::InvalidArgument);
        });
        assert_eq!(count, 1, "duplicate-eos redelivery must increment the counter");
    }
}
