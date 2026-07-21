//! Provider-neutral model discovery aggregation, merging, and caching.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::config::{ProviderAuth, ProviderEntry};

use super::{DiscoveredModel, ProviderBackend, default_model_for_alias};

const CACHE_TTL: Duration = Duration::from_secs(15 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CatalogSource {
    Suggested,
    Current,
    Configured,
    Remote,
}

impl CatalogSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::Remote => "live",
            Self::Configured => "configured",
            Self::Current => "current",
            Self::Suggested => "suggested",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelCatalogEntry {
    pub provider_alias: String,
    pub id: String,
    pub display_name: Option<String>,
    pub context_window: Option<u64>,
    pub supports_tools: Option<bool>,
    pub supports_images: Option<bool>,
    pub source: CatalogSource,
}

#[derive(Debug, Clone, Default)]
pub struct ModelCatalog {
    pub entries: Vec<ModelCatalogEntry>,
    pub notices: Vec<String>,
}

#[derive(Clone)]
struct CachedProvider {
    fetched_at: Instant,
    entries: Vec<ModelCatalogEntry>,
}

static CACHE: OnceLock<Mutex<HashMap<String, CachedProvider>>> = OnceLock::new();

/// Enumerate and discover providers concurrently, retaining successful partial
/// results and deterministic local fallbacks when individual endpoints fail.
pub async fn load_model_catalog(
    providers: HashMap<String, ProviderEntry>,
    default_auth: Option<ProviderAuth>,
    active_provider: String,
    active_model: String,
    force_refresh: bool,
) -> ModelCatalog {
    let aliases = discovery_aliases(
        &providers,
        &active_provider,
        super::providers_with_api_keys(),
    );
    let futures = aliases.iter().cloned().map(|alias| {
        let providers = providers.clone();
        async move { fetch_provider_catalog(&alias, &providers, default_auth, force_refresh).await }
    });
    let results = futures::future::join_all(futures).await;

    let mut entries = Vec::new();
    let mut notices = Vec::new();
    for (alias, result) in aliases.iter().zip(results) {
        match result {
            Ok(found) => entries.extend(found),
            Err(error) => {
                if let Some(stale) = cached(alias, false) {
                    notices.push(format!(
                        "{alias}: refresh failed ({error}); showing last successful catalog"
                    ));
                    entries.extend(stale);
                } else {
                    notices.push(format!("{alias}: discovery unavailable ({error})"));
                }
            }
        }
    }

    for alias in &aliases {
        if let Some(model) = providers
            .get(alias)
            .and_then(|entry| entry.model.as_deref())
        {
            entries.push(local_entry(alias, model, CatalogSource::Configured));
        }
        entries.push(local_entry(
            alias,
            default_model_for_alias(alias, &providers),
            CatalogSource::Suggested,
        ));
    }
    entries.push(local_entry(
        &active_provider,
        &active_model,
        CatalogSource::Current,
    ));

    let configured: HashSet<(String, String)> = providers
        .iter()
        .filter_map(|(alias, entry)| {
            entry
                .model
                .as_ref()
                .map(|model| (alias.to_ascii_lowercase(), model.clone()))
        })
        .collect();
    let active_key = (active_provider.to_ascii_lowercase(), active_model.clone());
    let mut entries = merge_entries(entries);
    entries.sort_by(|a, b| {
        rank(a, &active_key, &configured)
            .cmp(&rank(b, &active_key, &configured))
            .then_with(|| {
                a.provider_alias
                    .to_ascii_lowercase()
                    .cmp(&b.provider_alias.to_ascii_lowercase())
            })
            .then_with(|| a.id.to_ascii_lowercase().cmp(&b.id.to_ascii_lowercase()))
    });
    ModelCatalog { entries, notices }
}

/// Application-level provider enumeration remains owned here rather than by
/// backend resolution or the UI.
fn discovery_aliases<'a>(
    providers: &HashMap<String, ProviderEntry>,
    active_provider: &'a str,
    credentialed_builtins: impl IntoIterator<Item = &'a str>,
) -> Vec<String> {
    let mut aliases: Vec<String> = providers.keys().cloned().collect();
    for alias in std::iter::once(active_provider).chain(credentialed_builtins) {
        if !aliases
            .iter()
            .any(|known| known.eq_ignore_ascii_case(alias))
        {
            aliases.push(alias.to_string());
        }
    }
    aliases.sort_by_key(|alias| alias.to_ascii_lowercase());
    aliases
}

fn rank(
    entry: &ModelCatalogEntry,
    active: &(String, String),
    configured: &HashSet<(String, String)>,
) -> u8 {
    let key = (entry.provider_alias.to_ascii_lowercase(), entry.id.clone());
    if &key == active {
        0
    } else if configured.contains(&key) {
        1
    } else {
        match entry.source {
            CatalogSource::Remote => 2,
            CatalogSource::Configured => 1,
            CatalogSource::Current => 0,
            CatalogSource::Suggested => 3,
        }
    }
}

fn local_entry(alias: &str, id: &str, source: CatalogSource) -> ModelCatalogEntry {
    ModelCatalogEntry {
        provider_alias: alias.to_string(),
        id: id.to_string(),
        display_name: None,
        context_window: None,
        supports_tools: None,
        supports_images: None,
        source,
    }
}

fn remote_entry(alias: &str, discovered: DiscoveredModel) -> ModelCatalogEntry {
    ModelCatalogEntry {
        provider_alias: alias.to_string(),
        id: discovered.id,
        display_name: discovered.display_name,
        context_window: discovered.context_window,
        supports_tools: discovered.supports_tools,
        supports_images: discovered.supports_images,
        source: CatalogSource::Remote,
    }
}

fn merge_entries(entries: Vec<ModelCatalogEntry>) -> Vec<ModelCatalogEntry> {
    let mut merged: HashMap<(String, String), ModelCatalogEntry> = HashMap::new();
    for incoming in entries {
        let key = (
            incoming.provider_alias.to_ascii_lowercase(),
            incoming.id.clone(),
        );
        match merged.get_mut(&key) {
            None => {
                merged.insert(key, incoming);
            }
            Some(existing) => {
                if incoming.source > existing.source {
                    existing.source = incoming.source;
                }
                if existing.display_name.is_none() {
                    existing.display_name = incoming.display_name;
                }
                if existing.context_window.is_none() {
                    existing.context_window = incoming.context_window;
                }
                if existing.supports_tools.is_none() {
                    existing.supports_tools = incoming.supports_tools;
                }
                if existing.supports_images.is_none() {
                    existing.supports_images = incoming.supports_images;
                }
            }
        }
    }
    merged.into_values().collect()
}

async fn fetch_provider_catalog(
    alias: &str,
    providers: &HashMap<String, ProviderEntry>,
    default_auth: Option<ProviderAuth>,
    force_refresh: bool,
) -> anyhow::Result<Vec<ModelCatalogEntry>> {
    if !force_refresh && let Some(entries) = cached(alias, true) {
        return Ok(entries);
    }
    // Preserve the catalog's historical short notice for aliases that cannot
    // be resolved (including invalid provider definitions). Inference keeps
    // ProviderBackend's more actionable supported-provider message.
    if super::resolve_provider_info(alias, providers).is_none() {
        anyhow::bail!("unknown provider: {alias}")
    }
    let discovery = ProviderBackend::new(alias, providers, default_auth)?.model_discovery()?;
    let entries: Vec<ModelCatalogEntry> = discovery
        .list_models()
        .await?
        .into_iter()
        .map(|model| remote_entry(alias, model))
        .collect();
    CACHE
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(
            alias.to_ascii_lowercase(),
            CachedProvider {
                fetched_at: Instant::now(),
                entries: entries.clone(),
            },
        );
    Ok(entries)
}

fn cached(alias: &str, fresh_only: bool) -> Option<Vec<ModelCatalogEntry>> {
    let guard = CACHE
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let value = guard.get(&alias.to_ascii_lowercase())?;
    (!fresh_only || value.fetched_at.elapsed() < CACHE_TTL).then(|| value.entries.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalized_discovery_maps_to_remote_catalog_entry() {
        let row = remote_entry(
            "router",
            DiscoveredModel {
                id: "model-a".into(),
                display_name: Some("Model A".into()),
                context_window: Some(200_000),
                supports_tools: Some(true),
                supports_images: Some(false),
            },
        );
        assert_eq!(row.provider_alias, "router");
        assert_eq!(row.id, "model-a");
        assert_eq!(row.source, CatalogSource::Remote);
        assert_eq!(row.context_window, Some(200_000));
    }

    #[tokio::test]
    async fn unknown_provider_error_text_remains_stable() {
        let error =
            fetch_provider_catalog("definitely-unknown-provider", &HashMap::new(), None, true)
                .await
                .unwrap_err();
        assert_eq!(
            error.to_string(),
            "unknown provider: definitely-unknown-provider"
        );
    }

    #[test]
    fn merge_keeps_live_provenance_and_local_metadata() {
        let mut configured = local_entry("a", "m", CatalogSource::Configured);
        configured.context_window = Some(42);
        let remote = ModelCatalogEntry {
            provider_alias: "a".into(),
            id: "m".into(),
            display_name: Some("Model".into()),
            context_window: None,
            supports_tools: Some(true),
            supports_images: None,
            source: CatalogSource::Remote,
        };
        let rows = merge_entries(vec![configured, remote]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].source, CatalogSource::Remote);
        assert_eq!(rows[0].display_name.as_deref(), Some("Model"));
        assert_eq!(rows[0].context_window, Some(42));
    }

    #[test]
    fn current_and_configured_rank_before_live_and_suggested() {
        let active = ("a".to_string(), "current".to_string());
        let configured = HashSet::from([("a".to_string(), "pinned".to_string())]);
        assert_eq!(
            rank(
                &local_entry("a", "current", CatalogSource::Remote),
                &active,
                &configured
            ),
            0
        );
        assert_eq!(
            rank(
                &local_entry("a", "pinned", CatalogSource::Remote),
                &active,
                &configured
            ),
            1
        );
        assert_eq!(
            rank(
                &local_entry("a", "live", CatalogSource::Remote),
                &active,
                &configured
            ),
            2
        );
        assert_eq!(
            rank(
                &local_entry("a", "fallback", CatalogSource::Suggested),
                &active,
                &configured
            ),
            3
        );
    }

    #[test]
    fn discovery_includes_every_credentialed_builtin_without_config_entries() {
        let aliases = discovery_aliases(
            &HashMap::new(),
            "deepseek",
            ["deepseek", "openrouter", "gemini"],
        );
        assert_eq!(aliases, ["deepseek", "gemini", "openrouter"]);
    }

    #[test]
    fn discovery_preserves_named_aliases_and_deduplicates_case_insensitively() {
        let providers = HashMap::from([("My-Router".to_string(), ProviderEntry::default())]);
        let aliases = discovery_aliases(&providers, "my-router", ["openrouter"]);
        assert_eq!(aliases, ["My-Router", "openrouter"]);
    }

    #[test]
    fn cache_key_remains_lowercased_alias_only() {
        CACHE
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(
                "mixed-case".into(),
                CachedProvider {
                    fetched_at: Instant::now(),
                    entries: vec![local_entry("Mixed-Case", "m", CatalogSource::Remote)],
                },
            );
        assert_eq!(cached("MIXED-CASE", true).unwrap()[0].id, "m");
    }
}
