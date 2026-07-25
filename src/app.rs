use crate::{
    affinity::AffinityStore, backend::BackendRegistry, config::Config, metrics::Metrics,
    routing::Router,
};
use std::sync::Arc;

pub struct AppState {
    pub config: Arc<Config>,
    pub registry: Arc<BackendRegistry>,
    pub affinity: Arc<AffinityStore>,
    pub router: Arc<Router>,
    pub metrics: Arc<Metrics>,
}

impl AppState {
    pub fn from_config(config: Config) -> anyhow::Result<Arc<Self>> {
        let registry = Arc::new(BackendRegistry::from_config(&config)?);
        let affinity = Arc::new(AffinityStore::new(&config.affinity)?);
        let router = Arc::new(Router::new(
            registry.clone(),
            affinity.clone(),
            config.routing.clone(),
        ));
        Ok(Arc::new(Self {
            config: Arc::new(config),
            registry,
            affinity,
            router,
            metrics: Arc::new(Metrics::default()),
        }))
    }
}
