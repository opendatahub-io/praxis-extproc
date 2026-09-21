// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Red Hat, Inc.

//! Shared unit-test helpers for reading back recorded metrics.
//!
//! Compiled only under `cfg(test)`. Lets tests in any module install a
//! thread-local metrics recorder, run a closure, and assert on the counters it
//! emitted.

/// Value of the counter `name` carrying every `labels` pair (0 if absent).
pub(crate) fn snapshot_counter(
    snapshotter: &metrics_util::debugging::Snapshotter,
    name: &str,
    labels: &[(&str, &str)],
) -> u64 {
    use metrics_util::debugging::DebugValue;

    snapshotter
        .snapshot()
        .into_vec()
        .into_iter()
        .find_map(|(composite, _unit, _desc, value)| {
            let key = composite.key();
            let matches = key.name() == name
                && labels
                    .iter()
                    .all(|(k, v)| key.labels().any(|l| l.key() == *k && l.value() == *v));
            match value {
                DebugValue::Counter(count) if matches => Some(count),
                _ => None,
            }
        })
        .unwrap_or(0)
}

/// Run sync `f` under a thread-local recorder and read back counter `name`/`labels`.
pub(crate) fn counter_value(name: &str, labels: &[(&str, &str)], f: impl FnOnce()) -> u64 {
    use metrics_util::debugging::DebuggingRecorder;

    let recorder = DebuggingRecorder::new();
    let snapshotter = recorder.snapshotter();
    {
        // `::metrics` disambiguates the external crate from this crate's `metrics` module.
        let _guard = ::metrics::set_default_local_recorder(&recorder);
        f();
    }
    snapshot_counter(&snapshotter, name, labels)
}

/// Count `invalid_argument_total` increments matching `reason`/`detail` while `f` runs.
pub(crate) fn invalid_arg_count(reason: &str, detail: &str, f: impl FnOnce()) -> u64 {
    counter_value(
        "praxis_extproc_invalid_argument_total",
        &[("reason", reason), ("detail", detail)],
        f,
    )
}
