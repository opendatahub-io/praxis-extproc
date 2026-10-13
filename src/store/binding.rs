// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Red Hat, Inc.

//! Explicit database URL files, without YAML or environment interpolation.

use std::io::Read as _;

use praxis_core::config::{ChainRef, FilterEntry};

use crate::{
    config::ExtProcConfig,
    error::{ExtProcError, Result},
};

/// Bound connection material before deserializing it into secret-aware filters.
const MAX_URL_BYTES: u64 = 16_384;

/// Resolve only store filters, including nested chains and iterative steps.
pub(crate) fn resolve_config(config: &ExtProcConfig) -> Result<ExtProcConfig> {
    let mut resolved = config.clone();
    for chain in &mut resolved.filter_chains {
        resolve_entries(&mut chain.filters)?;
    }
    Ok(resolved)
}

/// Named chains are already roots; inline chains need explicit traversal.
fn resolve_entries(entries: &mut [FilterEntry]) -> Result<()> {
    for entry in entries {
        if is_store(&entry.filter_type) {
            resolve_url(&mut entry.config)?;
        }
        resolve_nested(&mut entry.config)?;
        for chain in entry
            .branch_chains
            .iter_mut()
            .flatten()
            .flat_map(|branch| &mut branch.chains)
        {
            if let ChainRef::Inline { filters, .. } = chain {
                resolve_entries(filters)?;
            }
        }
    }
    Ok(())
}

/// Iterative steps retain their filters as YAML until pipeline construction.
fn resolve_nested(value: &mut serde_yaml::Value) -> Result<()> {
    match value {
        serde_yaml::Value::Mapping(mapping) => {
            if mapping
                .get("filter")
                .and_then(serde_yaml::Value::as_str)
                .is_some_and(is_store)
            {
                resolve_url_mapping(mapping)?;
            }
            for value in mapping.values_mut() {
                resolve_nested(value)?;
            }
        },
        serde_yaml::Value::Sequence(values) => {
            for value in values {
                resolve_nested(value)?;
            }
        },
        _ => {},
    }
    Ok(())
}

/// Only these filters support the explicit connection-file option.
fn is_store(name: &str) -> bool {
    matches!(name, "openai_responses_store" | "openai_conversations")
}

/// Leave malformed mappings to ordinary filter validation.
fn resolve_url(value: &mut serde_yaml::Value) -> Result<()> {
    if let Some(mapping) = value.as_mapping_mut() {
        resolve_url_mapping(mapping)?;
    }
    Ok(())
}

/// Reject ambiguous sources and never embed file contents in error messages.
fn resolve_url_mapping(mapping: &mut serde_yaml::Mapping) -> Result<()> {
    let Some(path) = mapping.remove("database_url_file") else {
        return Ok(());
    };
    if mapping.contains_key("database_url") {
        return Err(ExtProcError::Config(
            "store requires exactly one database URL source".to_owned(),
        ));
    }
    let path = path
        .as_str()
        .filter(|path| !path.is_empty())
        .ok_or_else(|| ExtProcError::Config("database_url_file must be a nonempty path".to_owned()))?;
    let url = read_url(path)?;
    mapping.insert(serde_yaml::Value::from("database_url"), serde_yaml::Value::from(url));
    Ok(())
}

/// A mounted Secret is data, not executable YAML.
fn read_url(path: &str) -> Result<String> {
    let file = std::fs::File::open(path)
        .map_err(|error| ExtProcError::Config(format!("cannot open database_url_file: {error}")))?;
    let mut bytes = Vec::new();
    file.take(MAX_URL_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| ExtProcError::Config(format!("cannot read database_url_file: {error}")))?;
    if bytes.len() > usize::try_from(MAX_URL_BYTES).unwrap_or(usize::MAX) {
        return Err(ExtProcError::Config("database_url_file exceeds 16 KiB".to_owned()));
    }
    let url = String::from_utf8(bytes)
        .map_err(|_invalid_utf8| ExtProcError::Config("database_url_file is not UTF-8".to_owned()))?;
    let url = url.trim();
    if url.is_empty() || url.contains(['\n', '\r', '\0']) {
        return Err(ExtProcError::Config(
            "database_url_file must contain one nonempty URL".to_owned(),
        ));
    }
    Ok(url.to_owned())
}

#[cfg(test)]
#[expect(clippy::unwrap_used, clippy::indexing_slicing, reason = "tests")]
mod tests {
    use super::*;

    #[test]
    fn secret_file_is_literal_and_original_config_stays_reference_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("url");
        std::fs::write(&path, "postgres://role@db/database?options=${LITERAL}\n").unwrap();
        let config: ExtProcConfig = serde_yaml::from_str(&format!(
            "filter_chains: [{{name: main, filters: [{{filter: openai_responses_store, database_url_file: {:?}}}]}}]",
            path.to_str().unwrap()
        ))
        .unwrap();
        let resolved = resolve_config(&config).unwrap();
        assert_eq!(
            resolved.filter_chains[0].filters[0].config["database_url"],
            "postgres://role@db/database?options=${LITERAL}",
            "file contents are not interpolated"
        );
        assert!(
            config.filter_chains[0].filters[0].config.get("database_url").is_none(),
            "reference-only config must not retain connection material"
        );
    }

    #[test]
    fn invalid_sources_fail_without_disclosing_contents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("url");
        for contents in [
            String::new(),
            "secret\nsecond".to_owned(),
            "secret\0second".to_owned(),
            "x".repeat(16_385),
        ] {
            std::fs::write(&path, contents).unwrap();
            assert!(
                read_url(path.to_str().unwrap()).is_err(),
                "invalid file must fail closed"
            );
        }
        let mut config = serde_yaml::from_str("database_url: secret\ndatabase_url_file: missing").unwrap();
        let error = resolve_url(&mut config).unwrap_err().to_string();
        assert!(!error.contains("secret"), "errors must not disclose the inline URL");
        assert!(
            read_url("/nonexistent/praxis-test-url").is_err(),
            "missing binding must fail"
        );
        std::fs::write(&path, [0xFF]).unwrap();
        assert!(read_url(path.to_str().unwrap()).is_err(), "invalid UTF-8 must fail");
    }

    #[test]
    fn nested_steps_and_unrelated_filters_preserve_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("url");
        std::fs::write(&path, "sqlite::memory:").unwrap();
        let mut nested: serde_yaml::Value = serde_yaml::from_str(&format!(
            "steps: [{{filters: [{{filter: openai_conversations, database_url_file: {:?}}}, {{filter: headers, database_url_file: missing}}]}}]",
            path.to_str().unwrap()
        )).unwrap();
        resolve_nested(&mut nested).unwrap();
        assert_eq!(
            nested["steps"][0]["filters"][0]["database_url"], "sqlite::memory:",
            "nested stores resolve"
        );
        assert_eq!(
            nested["steps"][0]["filters"][1]["database_url_file"], "missing",
            "other filters cannot load files"
        );
    }
}
