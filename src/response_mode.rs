// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Red Hat, Inc.

//! Request-specific response-body mode selection for configured guardrail filters.

use std::collections::HashSet;

use praxis_core::config::{Condition, FilterChainConfig};
use praxis_proto::envoy::service::common::v3::HeaderValue;

use crate::adapter::header_value_str;

/// Conservative selector derived from configured `ai_guardrails` filter entries.
#[derive(Debug, Clone, Default)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "selector state keeps independent configuration validity and response-mode facts"
)]
pub struct ResponseModeSelector {
    /// Whether guardrail filters were found in the configuration.
    enabled: bool,
    /// Whether an unconditional guardrail is configured.
    ///
    /// This is used for FDS admission separately from response-mode selection:
    /// an unconditional input-only guardrail still cannot be enforced in FDS.
    has_unconditional_guardrail: bool,
    /// Whether an unconditional response-phase guardrail is configured.
    has_always_on_response_guardrail: bool,
    /// Header names that identify selected conditional response-phase guardrails.
    output_headers: HashSet<String>,
    /// Header names that identify any selected conditional guardrail phase.
    guardrail_headers: HashSet<String>,
    /// Whether all guardrail entries had a supported shape.
    valid: bool,
    /// Why the configured guardrail selector is invalid, when applicable.
    invalid_reason: Option<&'static str>,
}

impl ResponseModeSelector {
    /// Build a selector for unconditional filters and the exact header condition emitted by AIGC.
    #[expect(
        clippy::too_many_lines,
        reason = "guardrail entries are validated conservatively in one pass"
    )]
    pub fn from_config(chains: &[FilterChainConfig]) -> Self {
        let mut output_headers = HashSet::new();
        let mut guardrail_headers = HashSet::new();
        let mut has_unconditional_guardrail = false;
        let mut has_always_on_response_guardrail = false;
        let mut has_guardrail = false;
        for entry in chains.iter().flat_map(|chain| &chain.filters) {
            if entry.filter_type != "ai_guardrails" {
                continue;
            }
            has_guardrail = true;
            let Some((request_phase, response_phase)) = guardrail_phases(entry) else {
                return invalid_selector(
                    output_headers,
                    guardrail_headers,
                    "invalid guardrail phase configuration",
                );
            };
            if !request_phase && !response_phase {
                continue;
            }

            let Some(header) = selected_guardrail_header(entry) else {
                if entry.conditions.is_empty() && entry.response_conditions.is_empty() {
                    has_unconditional_guardrail = true;
                    has_always_on_response_guardrail |= response_phase;
                    continue;
                }
                return invalid_selector(
                    output_headers,
                    guardrail_headers,
                    "unsupported guardrail selection condition",
                );
            };
            guardrail_headers.insert(header.clone());
            if response_phase {
                output_headers.insert(header);
            }
        }

        if !has_guardrail {
            return Self::default();
        }

        Self {
            enabled: true,
            has_unconditional_guardrail,
            has_always_on_response_guardrail,
            output_headers,
            guardrail_headers,
            valid: true,
            invalid_reason: None,
        }
    }

    /// Return the startup diagnostic for an invalid guardrail selector.
    pub fn invalid_reason(&self) -> Option<&'static str> {
        self.invalid_reason
    }

    /// True when no selected output guardrail is present in the trusted headers.
    pub(crate) fn should_stream(&self, headers: &[HeaderValue]) -> bool {
        self.enabled
            && self.valid
            && !self.has_always_on_response_guardrail
            && !self
                .output_headers
                .iter()
                .any(|name| header_may_be_selected(headers, name))
    }

    /// True when a configured guardrail cannot be safely enforced in FDS mode.
    ///
    /// Invalid guardrail configuration is treated as selected here so
    /// FDS fails closed rather than allowing an unverified body through.
    pub(crate) fn has_selected_guardrail(&self, headers: &[HeaderValue]) -> bool {
        if !self.enabled {
            return false;
        }
        if !self.valid || self.has_unconditional_guardrail {
            return true;
        }
        self.guardrail_headers
            .iter()
            .any(|name| header_may_be_selected(headers, name))
    }
}

/// Build a selector that preserves buffering and rejects FDS when the config is invalid.
fn invalid_selector(
    output_headers: HashSet<String>,
    guardrail_headers: HashSet<String>,
    invalid_reason: &'static str,
) -> ResponseModeSelector {
    ResponseModeSelector {
        enabled: true,
        has_unconditional_guardrail: false,
        has_always_on_response_guardrail: false,
        output_headers,
        guardrail_headers,
        valid: false,
        invalid_reason: Some(invalid_reason),
    }
}

/// Parse the `ai_guardrails` phase configuration using the filter's defaults.
///
/// `PhaseConfig` defaults to request=true and response=false. The response-mode
/// selector mirrors those defaults because it receives the untyped filter entry
/// rather than the provider filter's deserialized configuration.
fn guardrail_phases(entry: &praxis_core::config::FilterEntry) -> Option<(bool, bool)> {
    let Some(phase) = entry.config.get("phase") else {
        return Some((true, false));
    };
    let mapping = phase.as_mapping()?;
    let request = mapping
        .get(serde_yaml::Value::from("request"))
        .map_or(Some(true), serde_yaml::Value::as_bool)?;
    let response = mapping
        .get(serde_yaml::Value::from("response"))
        .map_or(Some(false), serde_yaml::Value::as_bool)?;
    Some((request, response))
}

/// Extract the exact conditional selection header from a guardrail entry.
fn selected_guardrail_header(entry: &praxis_core::config::FilterEntry) -> Option<String> {
    if !entry.response_conditions.is_empty() {
        return None;
    }
    let [Condition::When(condition)] = entry.conditions.as_slice() else {
        return None;
    };
    if condition.grpc.is_some()
        || condition.path.is_some()
        || condition.path_prefix.is_some()
        || condition.methods.is_some()
        || condition.bound_upstream.is_some()
        || condition.selected_upstream.is_some()
    {
        return None;
    }
    let headers = condition.headers.as_ref()?;
    if headers.len() != 1 {
        return None;
    }
    let (name, value) = headers.iter().next()?;
    let name = name.to_ascii_lowercase();
    (value == "true" && name.starts_with("x-aigateway-guardrail-")).then_some(name)
}

/// Check whether a trusted header may select a guardrail, failing closed on duplicates.
fn header_may_be_selected(headers: &[HeaderValue], expected_name: &str) -> bool {
    let mut values = headers
        .iter()
        .filter(|header| header.key.eq_ignore_ascii_case(expected_name))
        .map(header_value_str);
    !matches!((values.next(), values.next()), (None, _) | (Some("false"), None))
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;

    fn header(key: &str, value: &str) -> HeaderValue {
        HeaderValue {
            key: key.to_owned(),
            value: value.to_owned(),
            raw_value: Vec::new(),
        }
    }

    fn raw_header(key: &str, value: &str, raw_value: &[u8]) -> HeaderValue {
        HeaderValue {
            key: key.to_owned(),
            value: value.to_owned(),
            raw_value: raw_value.to_vec(),
        }
    }

    fn selector(yaml: &str) -> ResponseModeSelector {
        let config: crate::config::ExtProcConfig = serde_yaml::from_str(yaml).unwrap();
        ResponseModeSelector::from_config(&config.filter_chains)
    }

    #[test]
    fn streams_when_only_input_guardrails_are_selected() {
        let selector = selector(
            r#"
filter_chains:
  - name: ipp
    filters:
      - filter: ai_guardrails
        conditions:
          - when:
              headers: { x-aigateway-guardrail-input: "true" }
        phase: { request: true, response: false }
      - filter: ai_guardrails
        conditions:
          - when:
              headers: { x-aigateway-guardrail-output: "true" }
        phase: { request: false, response: true }
"#,
        );
        assert!(selector.should_stream(&[header("x-aigateway-guardrail-input", "true")]));
    }

    #[test]
    fn streams_for_unconditional_input_only_guardrail_with_default_phase() {
        let selector = selector(
            "
filter_chains:
  - name: ipp
    filters:
      - filter: ai_guardrails
",
        );
        assert!(selector.should_stream(&[]));
    }

    #[test]
    fn keeps_buffered_mode_for_unconditional_response_guardrail() {
        let selector = selector(
            "
filter_chains:
  - name: ipp
    filters:
      - filter: ai_guardrails
        phase:
          response: true
",
        );
        assert!(!selector.should_stream(&[]));
    }

    #[test]
    fn streams_for_unconditional_input_and_unselected_dynamic_output() {
        let selector = selector(
            r#"
filter_chains:
  - name: ipp
    filters:
      - filter: ai_guardrails
        phase:
          request: true
          response: false
      - filter: ai_guardrails
        conditions:
          - when:
              headers: { x-aigateway-guardrail-output: "true" }
        phase:
          request: false
          response: true
"#,
        );
        assert!(selector.should_stream(&[]));
        assert!(selector.should_stream(&[header("x-aigateway-guardrail-output", "false")]));
    }

    #[test]
    fn partial_phase_uses_ai_guardrails_defaults() {
        let selector = selector(
            "
filter_chains:
  - name: ipp
    filters:
      - filter: ai_guardrails
        phase:
          response: false
",
        );
        assert!(selector.should_stream(&[]));
    }

    #[test]
    fn keeps_buffered_mode_when_an_output_guardrail_is_selected() {
        let selector = selector(
            r#"
filter_chains:
  - name: ipp
    filters:
      - filter: ai_guardrails
        conditions:
          - when:
              headers: { x-aigateway-guardrail-output: "true" }
        phase: { request: false, response: true }
"#,
        );
        assert!(!selector.should_stream(&[header("x-aigateway-guardrail-output", "true")]));
    }

    #[test]
    fn duplicate_output_header_keeps_buffered_mode() {
        let selector = selector(
            r#"
filter_chains:
  - name: ipp
    filters:
      - filter: ai_guardrails
        conditions:
          - when:
              headers: { x-aigateway-guardrail-output: "true" }
        phase: { request: false, response: true }
"#,
        );
        assert!(!selector.should_stream(&[
            header("x-aigateway-guardrail-output", "true"),
            header("x-aigateway-guardrail-output", "true"),
        ]));
    }

    #[test]
    fn streams_when_no_guardrail_header_is_selected() {
        let selector = selector(
            r#"
filter_chains:
  - name: ipp
    filters:
      - filter: ai_guardrails
        conditions:
          - when:
              headers: { x-aigateway-guardrail-output: "true" }
        phase: { request: false, response: true }
"#,
        );
        assert!(selector.should_stream(&[]));
        assert!(selector.should_stream(&[header("x-aigateway-guardrail-output", "false")]));
    }

    #[test]
    fn raw_false_guardrail_header_is_not_selected() {
        let selector = selector(
            r#"
filter_chains:
  - name: ipp
    filters:
      - filter: ai_guardrails
        conditions:
          - when:
              headers: { x-aigateway-guardrail-output: "true" }
        phase: { request: false, response: true }
"#,
        );
        let headers = [raw_header("x-aigateway-guardrail-output", "", b"false")];
        assert!(selector.should_stream(&headers));
        assert!(!selector.has_selected_guardrail(&headers));
    }

    #[test]
    fn raw_true_guardrail_header_is_selected() {
        let selector = selector(
            r#"
filter_chains:
  - name: ipp
    filters:
      - filter: ai_guardrails
        conditions:
          - when:
              headers: { x-aigateway-guardrail-output: "true" }
        phase: { request: false, response: true }
"#,
        );
        let headers = [raw_header("x-aigateway-guardrail-output", "", b"true")];
        assert!(!selector.should_stream(&headers));
        assert!(selector.has_selected_guardrail(&headers));
    }

    #[test]
    fn invalid_raw_guardrail_header_falls_back_to_value() {
        let selector = selector(
            r#"
filter_chains:
  - name: ipp
    filters:
      - filter: ai_guardrails
        conditions:
          - when:
              headers: { x-aigateway-guardrail-output: "true" }
        phase: { request: false, response: true }
"#,
        );
        let headers = [raw_header("x-aigateway-guardrail-output", "false", &[0xFF])];
        assert!(selector.should_stream(&headers));
        assert!(!selector.has_selected_guardrail(&headers));
    }

    #[test]
    fn selected_guardrail_is_not_allowed_in_fds() {
        let selector = selector(
            r#"
filter_chains:
  - name: ipp
    filters:
      - filter: ai_guardrails
        conditions:
          - when:
              headers: { x-aigateway-guardrail-input: "true" }
        phase: { request: true, response: false }
"#,
        );
        assert!(selector.has_selected_guardrail(&[header("x-aigateway-guardrail-input", "true")]));
        assert!(!selector.has_selected_guardrail(&[]));
    }

    #[test]
    fn unconditional_guardrail_is_not_allowed_in_fds() {
        let selector = selector(
            "
filter_chains:
  - name: ipp
    filters:
      - filter: ai_guardrails
        phase:
          request: true
          response: false
",
        );
        assert!(selector.should_stream(&[]));
        assert!(selector.has_selected_guardrail(&[]));
    }

    #[test]
    fn malformed_guardrail_phase_fails_closed_for_fds() {
        let selector = selector(
            "
filter_chains:
  - name: ipp
    filters:
      - filter: ai_guardrails
        phase:
          request: invalid
",
        );
        assert!(selector.has_selected_guardrail(&[]));
        assert!(!selector.should_stream(&[]));
        assert_eq!(selector.invalid_reason(), Some("invalid guardrail phase configuration"));
    }

    #[test]
    fn unsupported_guardrail_condition_reports_reason() {
        let selector = selector(
            "
filter_chains:
  - name: ipp
    filters:
      - filter: ai_guardrails
        conditions:
          - when:
              headers:
                x-aigateway-guardrail-a: \"true\"
                x-aigateway-guardrail-b: \"true\"
",
        );
        assert_eq!(
            selector.invalid_reason(),
            Some("unsupported guardrail selection condition")
        );
        assert!(!selector.should_stream(&[]));
        assert!(selector.has_selected_guardrail(&[]));
    }

    #[test]
    fn non_selection_header_condition_keeps_buffered_mode() {
        let selector = selector(
            r#"
filter_chains:
  - name: ipp
    filters:
      - filter: ai_guardrails
        conditions:
          - when:
              headers: { host: "true" }
        phase: { request: false, response: true }
"#,
        );

        assert_eq!(
            selector.invalid_reason(),
            Some("unsupported guardrail selection condition")
        );
        assert!(!selector.should_stream(&[]));
        assert!(selector.has_selected_guardrail(&[]));
    }

    #[test]
    fn keeps_existing_behavior_when_no_guardrail_filters_are_configured() {
        let selector = selector(
            "
filter_chains:
  - name: ipp
    filters:
      - filter: request_id
",
        );
        assert!(!selector.should_stream(&[]));
        assert!(!selector.has_selected_guardrail(&[]));
    }
}
