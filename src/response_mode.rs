// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Red Hat, Inc.

//! Request-specific response-body mode selection for generated guardrail filters.

use std::collections::HashSet;

use praxis_core::config::{Condition, FilterChainConfig};
use praxis_proto::envoy::service::common::v3::HeaderValue;

/// Conservative selector derived from generated `ai_guardrails` filter entries.
#[derive(Debug, Clone, Default)]
pub struct ResponseModeSelector {
    /// Whether generated guardrail filters were found in the configuration.
    enabled: bool,
    /// Header names that identify selected response-phase guardrails.
    output_headers: HashSet<String>,
    /// Header names that identify any selected guardrail phase.
    guardrail_headers: HashSet<String>,
    /// Whether all generated guardrail entries had a supported shape.
    valid: bool,
}

impl ResponseModeSelector {
    /// Build a selector only for the exact header condition emitted by AIGC.
    #[expect(
        clippy::too_many_lines,
        reason = "guardrail entries are validated conservatively in one pass"
    )]
    pub fn from_config(chains: &[FilterChainConfig]) -> Self {
        let mut output_headers = HashSet::new();
        let mut guardrail_headers = HashSet::new();
        let mut has_guardrail = false;
        for entry in chains.iter().flat_map(|chain| &chain.filters) {
            if entry.filter_type != "ai_guardrails" {
                continue;
            }
            has_guardrail = true;
            let Some(phase) = entry.config.get("phase") else {
                return invalid_selector(output_headers, guardrail_headers);
            };
            let Some(request_phase) = phase.get("request").and_then(serde_yaml::Value::as_bool) else {
                return invalid_selector(output_headers, guardrail_headers);
            };
            let Some(response_phase) = phase.get("response").and_then(serde_yaml::Value::as_bool) else {
                return invalid_selector(output_headers, guardrail_headers);
            };
            if !request_phase && !response_phase {
                continue;
            }

            let Some(header) = selected_guardrail_header(entry) else {
                return invalid_selector(output_headers, guardrail_headers);
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
            output_headers,
            guardrail_headers,
            valid: true,
        }
    }

    /// True when no selected output guardrail is present in the trusted headers.
    pub(crate) fn should_stream(&self, headers: &[HeaderValue]) -> bool {
        self.enabled
            && self.valid
            && !self
                .output_headers
                .iter()
                .any(|name| header_may_be_selected(headers, name))
    }

    /// True when a selected guardrail cannot be safely enforced in FDS mode.
    ///
    /// Invalid generated guardrail configuration is treated as selected here so
    /// FDS fails closed rather than allowing an unverified body through.
    pub(crate) fn has_selected_guardrail(&self, headers: &[HeaderValue]) -> bool {
        if !self.enabled {
            return false;
        }
        if !self.valid {
            return true;
        }
        self.guardrail_headers
            .iter()
            .any(|name| header_may_be_selected(headers, name))
    }
}

/// Build a selector that preserves buffering and rejects FDS when the config is invalid.
fn invalid_selector(output_headers: HashSet<String>, guardrail_headers: HashSet<String>) -> ResponseModeSelector {
    ResponseModeSelector {
        enabled: true,
        output_headers,
        guardrail_headers,
        valid: false,
    }
}

/// Extract the exact generated selection header from a guardrail entry.
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
    (value == "true").then(|| name.to_ascii_lowercase())
}

/// Check whether a trusted header may select a guardrail, failing closed on duplicates.
fn header_may_be_selected(headers: &[HeaderValue], expected_name: &str) -> bool {
    let mut values = headers
        .iter()
        .filter(|header| header.key.eq_ignore_ascii_case(expected_name))
        .map(|header| header.value.as_str());
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
    fn malformed_guardrail_phase_fails_closed_for_fds() {
        let selector = selector(
            r#"
filter_chains:
  - name: ipp
    filters:
      - filter: ai_guardrails
        conditions:
          - when:
              headers: { x-aigateway-guardrail-input: "true" }
"#,
        );
        assert!(selector.has_selected_guardrail(&[]));
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
        assert!(!selector.has_selected_guardrail(&[]));
    }
}
