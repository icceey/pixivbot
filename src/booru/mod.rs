//! Shared registry of configured Booru sites.
//!
//! Centralises [`BooruClient`] construction so the scheduler engine and bot
//! command handlers operate on the same set of sites without each component
//! re-building its own [`BooruClient`] from raw config. The registry is
//! built once at startup and shared via [`Arc`] across the bot and engines.

use crate::config::BooruSiteConfig;
use booru_client::BooruClient;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{error, info};

pub struct BooruSite {
    pub client: BooruClient,
    pub config: BooruSiteConfig,
}

#[derive(Default)]
pub struct BooruSiteRegistry {
    sites: HashMap<String, Arc<BooruSite>>,
}

impl BooruSiteRegistry {
    pub fn from_configs(configs: &[BooruSiteConfig]) -> Arc<Self> {
        let mut sites = HashMap::new();
        for cfg in configs {
            let client = match BooruClient::new(&cfg.base_url, cfg.engine_type) {
                Ok(c) => c,
                Err(e) => {
                    error!("Failed to create BooruClient for {}: {:#}", cfg.name, e);
                    continue;
                }
            };
            let client = match (&cfg.username, &cfg.api_key) {
                (Some(user), Some(key)) => client.with_auth(user, key),
                _ => client,
            };
            let client = match &cfg.bypass {
                Some(bypass_cfg) => client.with_bypass(bypass_cfg.to_client_config()),
                None => client,
            };
            sites.insert(
                cfg.name.to_lowercase(),
                Arc::new(BooruSite {
                    client,
                    config: cfg.clone(),
                }),
            );
        }
        info!("Booru registry built with {} site(s)", sites.len());
        Arc::new(Self { sites })
    }

    /// Lookup by site name (case-insensitive).
    pub fn get(&self, name: &str) -> Option<&Arc<BooruSite>> {
        self.sites.get(&name.to_lowercase())
    }

    pub fn iter(&self) -> impl Iterator<Item = &Arc<BooruSite>> {
        self.sites.values()
    }

    pub fn is_empty(&self) -> bool {
        self.sites.is_empty()
    }

    pub fn len(&self) -> usize {
        self.sites.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use booru_client::BooruEngineType;

    fn make_cfg(name: &str) -> BooruSiteConfig {
        BooruSiteConfig {
            name: name.to_string(),
            engine_type: BooruEngineType::Moebooru,
            base_url: "https://example.com".to_string(),
            username: None,
            api_key: None,
            min_interval_sec: 1800,
            max_interval_sec: 3600,
            page_limit: 20,
            bypass: None,
        }
    }

    #[test]
    fn registry_lookup_is_case_insensitive() {
        let registry = BooruSiteRegistry::from_configs(&[make_cfg("Konachan")]);
        assert!(registry.get("konachan").is_some());
        assert!(registry.get("KONACHAN").is_some());
        assert!(registry.get("Konachan").is_some());
        assert!(registry.get("missing").is_none());
        assert_eq!(registry.len(), 1);
        assert!(!registry.is_empty());
    }

    #[test]
    fn registry_iter_yields_all_sites() {
        let registry = BooruSiteRegistry::from_configs(&[make_cfg("a"), make_cfg("b")]);
        let names: Vec<String> = registry
            .iter()
            .map(|s| s.config.name.clone())
            .collect::<Vec<_>>();
        assert_eq!(names.len(), 2);
    }

    #[test]
    fn registry_default_empty() {
        let registry = BooruSiteRegistry::default();
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
    }
}
