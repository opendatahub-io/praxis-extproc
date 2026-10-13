// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Red Hat, Inc.

//! Serving-runtime initialization of persisted-state stores.

mod plan;

use std::sync::Arc;

use praxis_ai_apis::store::ResponseStoreRegistry;
use praxis_ai_store::{StoreBackendFactory, StoreRegistry};
use praxis_ai_store_lifecycle::{BackendCache, BackendLease, StoreRef};
use praxis_filter::FilterPipeline;

use crate::{
    config::ExtProcConfig,
    error::{ExtProcError, Result},
};

/// Validated stores and the registry installed into a pipeline.
pub(crate) struct StoreBootstrap {
    /// Lifecycle cache owning backend initialization and retry policy.
    cache: BackendCache,

    /// Store references required by the pipeline.
    refs: Vec<StoreRef>,

    /// Shared request-time registry, unready until provisioning succeeds.
    registry: StoreRegistry,
}

impl StoreBootstrap {
    /// Validate configured stores without connecting to a database.
    ///
    /// # Errors
    ///
    /// Rejects malformed, conflicting or unavailable backend configurations.
    pub(crate) fn new(config: &ExtProcConfig) -> Result<Self> {
        let factories = backend_factories();
        let refs = plan::store_refs(config, &factories)?;
        let cache = BackendCache::new(factories);
        cache.validate(&refs).map_err(|e| ExtProcError::Config(e.to_string()))?;

        Ok(Self {
            cache,
            refs,
            registry: StoreRegistry::new(),
        })
    }

    /// Install the shared registry into the pipeline.
    pub(crate) fn attach(&self, pipeline: &mut FilterPipeline) {
        pipeline.add_pipeline_extension(Box::new(ResponseStoreRegistry::from(self.registry.clone())));
    }

    /// Open and register every backend on the serving runtime.
    ///
    /// Keep the lease until in-flight requests drain, then call `release`.
    ///
    /// # Errors
    ///
    /// Returns an error after the lifecycle library's bounded retry budget.
    pub(crate) async fn provision(&self) -> Result<BackendLease> {
        let lease = self
            .cache
            .provision_into(&self.refs, &self.registry)
            .await
            .map_err(|e| ExtProcError::Pipeline(e.to_string()))?;
        self.registry.mark_ready();
        Ok(lease)
    }
}

/// Factories provided by the selected backend feature profile.
fn backend_factories() -> Vec<Arc<dyn StoreBackendFactory>> {
    #[cfg(any(feature = "responses-full", feature = "store-postgres-cert-auth"))]
    {
        praxis_ai_apis::store::store_backend_factories()
    }
    #[cfg(not(any(feature = "responses-full", feature = "store-postgres-cert-auth")))]
    {
        Vec::new()
    }
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests")]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use praxis_ai_store::{BackendError, EffectiveConfigKey, ProvisionedBackend};
    use serde_json::json;

    use super::*;

    #[tokio::test]
    async fn stateless_bootstrap_stays_offline_until_provisioned() {
        let config = serde_yaml::from_str("filter_chains: []").unwrap();
        let bootstrap = StoreBootstrap::new(&config).unwrap();
        assert!(
            !bootstrap.registry.is_ready(),
            "offline construction cannot advertise store readiness"
        );
        let lease = bootstrap.provision().await.unwrap();
        assert!(bootstrap.registry.is_ready(), "stateless bootstrap needs no database");
        lease.release().await;
    }

    #[tokio::test]
    async fn unavailable_store_never_marks_registry_ready() {
        let calls = Arc::new(AtomicUsize::new(0));
        let bootstrap = StoreBootstrap {
            cache: BackendCache::new(vec![Arc::new(FailingFactory {
                calls: Arc::clone(&calls),
            })]),
            refs: vec![StoreRef {
                name: Arc::from("default"),
                backend_id: Arc::from("failing"),
                config: json!({}),
            }],
            registry: StoreRegistry::new(),
        };
        let error = bootstrap.provision().await.err().unwrap();
        assert!(
            error.to_string().contains("after 3 attempts"),
            "must use lifecycle retry budget: {error}"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "provisioning must not retry indefinitely"
        );
        assert!(
            !bootstrap.registry.is_ready(),
            "failed provisioning must not admit traffic"
        );
        assert!(
            !bootstrap.registry.contains("default"),
            "failed initialization must not publish a backend"
        );
    }

    // -----------------------------------------------------------------------------
    // Test Utilities
    // -----------------------------------------------------------------------------

    /// Unreachable backend recording the lifecycle retry budget.
    struct FailingFactory {
        /// Number of attempts to initialize the backend.
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl StoreBackendFactory for FailingFactory {
        fn backend_id(&self) -> &str {
            "failing"
        }

        fn effective_key(&self, _config: &serde_json::Value) -> std::result::Result<EffectiveConfigKey, BackendError> {
            Ok(EffectiveConfigKey::new("failing"))
        }

        async fn build(&self, _config: &serde_json::Value) -> std::result::Result<ProvisionedBackend, BackendError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(BackendError::Transient("test database unreachable".to_owned()))
        }
    }
}
