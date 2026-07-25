use crate::{
    affinity::{AffinityKey, AffinityStore},
    backend::{BackendHealth, BackendRegistry, BackendRuntime},
    config::{AffinityBusyPolicy, BackendKind, RoutingConfig},
    error::ProxyError,
};
use std::{cmp::Ordering, collections::HashSet, sync::Arc};
use tokio::sync::OwnedSemaphorePermit;

#[derive(Debug, Clone, Copy)]
pub enum RoutingReason {
    AffinityHit,
    AffinityReassigned,
    LocalFirst,
    RemoteFallback,
    DistributedPreferred,
    LeastLoaded,
    OnlyAvailable,
    RetryDifferentBackend,
}

impl RoutingReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AffinityHit => "affinity_hit",
            Self::AffinityReassigned => "affinity_reassigned",
            Self::LocalFirst => "local_first",
            Self::RemoteFallback => "remote_fallback",
            Self::DistributedPreferred => "distributed_preferred",
            Self::LeastLoaded => "least_loaded",
            Self::OnlyAvailable => "only_available",
            Self::RetryDifferentBackend => "retry_different_backend",
        }
    }
}

pub struct Selection {
    pub backend: Arc<BackendRuntime>,
    pub permit: OwnedSemaphorePermit,
    pub reason: RoutingReason,
    pub affinity_hit: bool,
}

pub struct Router {
    registry: Arc<BackendRegistry>,
    affinity: Arc<AffinityStore>,
    config: RoutingConfig,
}

impl Router {
    pub fn new(
        registry: Arc<BackendRegistry>,
        affinity: Arc<AffinityStore>,
        config: RoutingConfig,
    ) -> Self {
        Self {
            registry,
            affinity,
            config,
        }
    }

    pub async fn select(
        &self,
        key: Option<&AffinityKey>,
        excluded: &HashSet<String>,
        inference: bool,
        retry: bool,
    ) -> Result<Selection, ProxyError> {
        let mut stale_affinity = false;
        if inference
            && let Some(key) = key
            && let Some(entry) = self.affinity.lookup(key)
        {
            if !excluded.contains(&entry.backend_id)
                && let Some(backend) = self.registry.by_id(&entry.backend_id)
                && backend.snapshot().health == BackendHealth::Alive
            {
                if let Some(permit) = backend.try_acquire(true) {
                    return Ok(Selection {
                        backend,
                        permit,
                        reason: RoutingReason::AffinityHit,
                        affinity_hit: true,
                    });
                }
                if self.config.affinity_busy_policy == AffinityBusyPolicy::Fail {
                    if let Some(permit) = backend
                        .acquire_affinity_slot(self.config.affinity_busy_grace)
                        .await
                    {
                        if backend.is_alive() {
                            return Ok(Selection {
                                backend,
                                permit,
                                reason: RoutingReason::AffinityHit,
                                affinity_hit: true,
                            });
                        }
                        drop(permit);
                    }
                    if backend.is_alive() {
                        return Err(ProxyError::NoBackendAvailable);
                    }
                }
            }
            stale_affinity = true;
        }

        let mut candidates = self
            .registry
            .all()
            .iter()
            .filter(|backend| !excluded.contains(&backend.config.id))
            .filter(|backend| {
                if inference {
                    backend.is_available()
                } else {
                    backend.is_alive()
                }
            })
            .cloned()
            .collect::<Vec<_>>();
        candidates.sort_by(|left, right| compare(left, right, key.is_some(), &self.config));
        let candidate_count = candidates.len();
        for backend in candidates {
            if let Some(permit) = backend.try_acquire(inference) {
                let reason = if retry {
                    RoutingReason::RetryDifferentBackend
                } else if stale_affinity {
                    RoutingReason::AffinityReassigned
                } else if candidate_count == 1 {
                    RoutingReason::OnlyAvailable
                } else if key.is_some()
                    && self.config.prefer_distributed_for_affinity
                    && backend.config.kind == BackendKind::Distributed
                {
                    RoutingReason::DistributedPreferred
                } else if backend.config.local {
                    RoutingReason::LocalFirst
                } else if backend.load_ratio() > 0.0 {
                    RoutingReason::LeastLoaded
                } else {
                    RoutingReason::RemoteFallback
                };
                return Ok(Selection {
                    backend,
                    permit,
                    reason,
                    affinity_hit: false,
                });
            }
        }
        Err(ProxyError::NoBackendAvailable)
    }
}

fn compare(
    left: &BackendRuntime,
    right: &BackendRuntime,
    has_affinity: bool,
    config: &RoutingConfig,
) -> Ordering {
    let distributed_left = has_affinity
        && config.prefer_distributed_for_affinity
        && left.config.kind == BackendKind::Distributed;
    let distributed_right = has_affinity
        && config.prefer_distributed_for_affinity
        && right.config.kind == BackendKind::Distributed;
    distributed_right
        .cmp(&distributed_left)
        .then_with(|| right.config.local.cmp(&left.config.local))
        .then_with(|| right.config.priority.cmp(&left.config.priority))
        .then_with(|| {
            left.load_ratio()
                .partial_cmp(&right.load_ratio())
                .unwrap_or(Ordering::Equal)
        })
        .then_with(|| {
            let left_latency = left.snapshot().ewma_latency_ms.unwrap_or(f64::INFINITY);
            let right_latency = right.snapshot().ewma_latency_ms.unwrap_or(f64::INFINITY);
            left_latency
                .partial_cmp(&right_latency)
                .unwrap_or(Ordering::Equal)
        })
        .then_with(|| left.config.id.cmp(&right.config.id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{affinity::AffinitySource, backend::BackendRegistry, config::Config};

    fn components() -> (Arc<BackendRegistry>, Arc<AffinityStore>, RoutingConfig) {
        let config: Config = toml::from_str(
            r#"
listen = "127.0.0.1:18080"
admin_listen = "127.0.0.1:18081"

[affinity]
enabled = true

[[backends]]
id = "local"
url = "http://127.0.0.1:8000"
local = true
priority = 100

[[backends]]
id = "remote"
url = "http://127.0.0.1:8001"
priority = 50
"#,
        )
        .unwrap();
        let registry = Arc::new(BackendRegistry::from_config(&config).unwrap());
        for backend in registry.all() {
            backend.record_heartbeat_success();
        }
        let affinity = Arc::new(AffinityStore::with_secret(&config.affinity, vec![1; 32]).unwrap());
        (registry, affinity, config.routing)
    }

    #[tokio::test]
    async fn local_first_without_affinity() {
        let (registry, affinity, config) = components();
        let router = Router::new(registry, affinity, config);
        let selection = router
            .select(None, &HashSet::new(), true, false)
            .await
            .unwrap();
        assert_eq!(selection.backend.config.id, "local");
        assert!(matches!(selection.reason, RoutingReason::LocalFirst));
    }

    #[tokio::test]
    async fn existing_affinity_beats_local_first() {
        let (registry, affinity, config) = components();
        let key = AffinityKey {
            hash: [9; 32],
            source: AffinitySource::Explicit,
            tag: "090909090909".into(),
        };
        affinity.assign(&key, "remote");
        let router = Router::new(registry, affinity, config);
        let selection = router
            .select(Some(&key), &HashSet::new(), true, false)
            .await
            .unwrap();
        assert_eq!(selection.backend.config.id, "remote");
        assert!(selection.affinity_hit);
    }
}
