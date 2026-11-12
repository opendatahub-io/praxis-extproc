// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Red Hat, Inc.

//! Request-specific response-body mode selection for configured guardrail filters.

use std::collections::HashSet;

use praxis_core::config::{Condition, FilterChainConfig, FilterEntry};
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
    /// Header names that select a guardrail requiring a complete request body.
    guardrail_headers: HashSet<String>,
    /// Whether an unconditional response-phase guardrail is configured.
    has_always_on_response_guardrail: bool,
    /// Whether any active guardrail has no selection condition.
    has_unconditional_guardrail: bool,
    /// Header names that identify selected conditional response-phase guardrails.
    output_headers: HashSet<String>,
    /// Whether all guardrail entries had a supported shape.
    valid: bool,
    /// Why the configured guardrail selector is invalid, when applicable.
    invalid_reason: Option<&'static str>,
}

impl ResponseModeSelector {
    /// Conservatively require complete requests when an embedded pipeline has guardrails.
    ///
    /// A pipeline does not expose the original phase config. Until an explicit
    /// selector is supplied, avoid guessing which requests or phases are selected.
    pub(crate) fn for_pipeline(pipeline: &praxis_filter::FilterPipeline) -> Self {
        Self {
            enabled: pipeline.contains_filter("ai_guardrails"),
            ..Self::default()
        }
    }

    /// Build a selector for unconditional filters and the exact header condition emitted by AIGC.
    pub fn from_config(chains: &[FilterChainConfig]) -> Self {
        let mut selector = Self {
            valid: true,
            ..Self::default()
        };
        for entry in chains
            .iter()
            .flat_map(|chain| &chain.filters)
            .filter(|entry| entry.filter_type == "ai_guardrails")
        {
            selector.enabled = true;
            if let Err(reason) = selector.add_guardrail(entry) {
                selector.valid = false;
                selector.invalid_reason = Some(reason);
                break;
            }
        }
        selector
    }

    /// Return the startup diagnostic for an invalid guardrail selector.
    pub fn invalid_reason(&self) -> Option<&'static str> {
        self.invalid_reason
    }

    /// Whether guardrails exist but their selection cannot be inferred safely.
    pub(crate) fn selection_unknown(&self) -> bool {
        self.enabled && !self.valid
    }

    /// Whether a selected guardrail needs complete request-body delivery.
    ///
    /// Unknown selection shapes are conservative because output screening also
    /// depends on the request-body hook, even without an input check.
    pub(crate) fn requires_complete_request(&self, headers: &[HeaderValue]) -> bool {
        self.enabled
            && (!self.valid
                || self.has_unconditional_guardrail
                || self
                    .guardrail_headers
                    .iter()
                    .any(|name| header_may_be_selected(headers, name)))
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

    /// Add the supported phase and selection facts for one guardrail entry.
    fn add_guardrail(&mut self, entry: &FilterEntry) -> Result<(), &'static str> {
        let (request_phase, response_phase) = guardrail_phases(entry).ok_or("invalid guardrail phase configuration")?;
        if !request_phase && !response_phase {
            return Ok(());
        }
        if entry.conditions.is_empty() && entry.response_conditions.is_empty() {
            self.has_unconditional_guardrail = true;
            self.has_always_on_response_guardrail |= response_phase;
            return Ok(());
        }
        let header = selected_guardrail_header(entry).ok_or("unsupported guardrail selection condition")?;
        self.guardrail_headers.insert(header.clone());
        if response_phase {
            self.output_headers.insert(header);
        }
        Ok(())
    }
}

/// Parse the `ai_guardrails` phase configuration using the filter's defaults.
///
/// `PhaseConfig` defaults to request=true and response=false. The response-mode
/// selector mirrors those defaults because it receives the untyped filter entry
/// rather than the provider filter's deserialized configuration.
fn guardrail_phases(entry: &FilterEntry) -> Option<(bool, bool)> {
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
fn selected_guardrail_header(entry: &FilterEntry) -> Option<String> {
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

    #[test]
    fn complete_request_requirement_tracks_active_and_unknown_guardrails() {
        for (entry, required) in [
            ("filter: request_id", false),
            ("filter: ai_guardrails", true),
            (
                "filter: ai_guardrails\n        phase: { request: false, response: true }",
                true,
            ),
            (
                "filter: ai_guardrails\n        phase: { request: false, response: false }",
                false,
            ),
            ("filter: ai_guardrails\n        phase: invalid", true),
            (
                "filter: ai_guardrails\n        conditions:\n          - when:\n              headers: { host: model.test }",
                true,
            ),
        ] {
            let selector = selector(&format!(
                "filter_chains:\n  - name: main\n    filters:\n      - {entry}\n"
            ));
            assert_eq!(selector.requires_complete_request(&[]), required, "{entry}");
        }
        assert!(!ResponseModeSelector::default().requires_complete_request(&[]));
    }

    #[test]
    fn complete_request_selection_honors_raw_values_and_missing_headers() {
        let selector = selector(
            "filter_chains:\n  - name: main\n    filters:\n      - filter: ai_guardrails\n        conditions:\n          - when:\n              headers: { x-aigateway-guardrail-input: 'true' }\n",
        );
        assert!(!selector.requires_complete_request(&[]));
        for (text, raw, selected) in [
            ("true", "false", false),
            ("false", "true", true),
            ("false", "", false),
            ("true", "", true),
        ] {
            assert_eq!(
                selector.requires_complete_request(&[raw_header("x-aigateway-guardrail-input", text, raw.as_bytes())]),
                selected,
                "text={text}, raw={raw}"
            );
        }
        assert!(selector.requires_complete_request(&[
            header("x-aigateway-guardrail-input", "false"),
            header("x-aigateway-guardrail-input", "true")
        ]));
    }

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
    }

    #[test]
    fn malformed_guardrail_phase_keeps_buffered_mode() {
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
    }
}
