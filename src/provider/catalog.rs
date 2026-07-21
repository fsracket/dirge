//! Provider-neutral model discovery, merging, and process-local caching.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use futures::StreamExt;
use serde_json::Value;

use crate::config::{ProviderAuth, ProviderEntry};

use super::client::{CatalogTransport, create_catalog_transport_with_auth};
use super::{ProviderKind, default_model_for_alias};

const CACHE_TTL: Duration = Duration::from_secs(15 * 60);
const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;
const MAX_PAGES: usize = 10;

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

/// Discover every configured provider concurrently, retaining successful
/// partial results and deterministic local fallbacks when endpoints fail.
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
            Err(e) => {
                if let Some(stale) = cached(alias, false) {
                    notices.push(format!(
                        "{alias}: refresh failed ({e}); showing last successful catalog"
                    ));
                    entries.extend(stale);
                } else {
                    notices.push(format!("{alias}: discovery unavailable ({e})"));
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
    let transport = create_catalog_transport_with_auth(alias, providers, default_auth)?;
    let mut entries = match transport.kind {
        ProviderKind::Anthropic => fetch_anthropic(&transport).await?,
        ProviderKind::Gemini => fetch_gemini(&transport).await?,
        ProviderKind::Ollama => fetch_ollama(&transport).await?,
        _ => fetch_openai_compatible(&transport).await?,
    };
    for entry in &mut entries {
        entry.provider_alias = alias.to_string();
    }
    CACHE
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
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
        .unwrap_or_else(|e| e.into_inner());
    let value = guard.get(&alias.to_ascii_lowercase())?;
    (!fresh_only || value.fetched_at.elapsed() < CACHE_TTL).then(|| value.entries.clone())
}

async fn fetch_openai_compatible(
    transport: &CatalogTransport,
) -> anyhow::Result<Vec<ModelCatalogEntry>> {
    let json = get_json(transport, format!("{}/models", transport.base_url)).await?;
    parse_openai_models(&json)
}

async fn fetch_ollama(transport: &CatalogTransport) -> anyhow::Result<Vec<ModelCatalogEntry>> {
    let base = transport.base_url.trim_end_matches("/api");
    let json = get_json(transport, format!("{base}/api/tags")).await?;
    parse_ollama_models(&json)
}

fn parse_ollama_models(json: &Value) -> anyhow::Result<Vec<ModelCatalogEntry>> {
    let rows = json
        .get("models")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("response has no models array"))?;
    Ok(rows
        .iter()
        .filter_map(|row| row.get("name").and_then(Value::as_str))
        .map(|id| remote_entry(id, None, None, None, None))
        .collect())
}

async fn fetch_anthropic(transport: &CatalogTransport) -> anyhow::Result<Vec<ModelCatalogEntry>> {
    let base = transport.base_url.trim_end_matches("/v1");
    let endpoint = format!("{base}/v1/models");
    let mut after_id: Option<String> = None;
    let mut out = Vec::new();
    for _ in 0..MAX_PAGES {
        let mut url = reqwest::Url::parse(&endpoint)?;
        url.query_pairs_mut().append_pair("limit", "100");
        if let Some(token) = after_id.as_deref() {
            url.query_pairs_mut().append_pair("after_id", token);
        }
        let json = get_json(transport, url).await?;
        out.extend(parse_anthropic_models(&json)?);
        if !json
            .get("has_more")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return Ok(out);
        }
        after_id = Some(
            json.get("last_id")
                .and_then(Value::as_str)
                .filter(|token| !token.is_empty())
                .ok_or_else(|| anyhow::anyhow!("pagination response omitted last_id"))?
                .to_string(),
        );
    }
    anyhow::bail!("catalog pagination exceeded {MAX_PAGES} pages")
}

fn parse_anthropic_models(json: &Value) -> anyhow::Result<Vec<ModelCatalogEntry>> {
    let rows = json
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("response has no data array"))?;
    Ok(rows
        .iter()
        .filter_map(|row| {
            let id = row.get("id")?.as_str()?;
            let name = row
                .get("display_name")
                .and_then(Value::as_str)
                .map(str::to_string);
            Some(remote_entry(id, name, None, None, None))
        })
        .collect())
}

async fn fetch_gemini(transport: &CatalogTransport) -> anyhow::Result<Vec<ModelCatalogEntry>> {
    let base = transport.base_url.trim_end_matches("/v1beta");
    let mut page_token: Option<String> = None;
    let mut out = Vec::new();
    for _ in 0..MAX_PAGES {
        let mut url = reqwest::Url::parse(&format!("{base}/v1beta/models"))?;
        url.query_pairs_mut().append_pair("pageSize", "1000");
        if let Some(token) = page_token.as_deref() {
            url.query_pairs_mut().append_pair("pageToken", token);
        }
        let json = get_json(transport, url).await?;
        out.extend(parse_gemini_models(&json)?);
        page_token = json
            .get("nextPageToken")
            .and_then(Value::as_str)
            .filter(|token| !token.is_empty())
            .map(str::to_string);
        if page_token.is_none() {
            return Ok(out);
        }
    }
    anyhow::bail!("catalog pagination exceeded {MAX_PAGES} pages")
}

fn parse_gemini_models(json: &Value) -> anyhow::Result<Vec<ModelCatalogEntry>> {
    let rows = json
        .get("models")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("response has no models array"))?;
    Ok(rows
        .iter()
        .filter_map(|row| {
            let methods = row
                .get("supportedGenerationMethods")
                .and_then(Value::as_array)?;
            if !methods
                .iter()
                .any(|m| m.as_str() == Some("generateContent"))
            {
                return None;
            }
            let id = row.get("name")?.as_str()?.trim_start_matches("models/");
            let name = row
                .get("displayName")
                .and_then(Value::as_str)
                .map(str::to_string);
            let context = row.get("inputTokenLimit").and_then(Value::as_u64);
            Some(remote_entry(id, name, context, None, None))
        })
        .collect())
}

fn parse_openai_models(json: &Value) -> anyhow::Result<Vec<ModelCatalogEntry>> {
    let rows = json
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("response has no data array"))?;
    Ok(rows
        .iter()
        .filter_map(|row| {
            let id = row.get("id")?.as_str()?;
            let name = row
                .get("name")
                .or_else(|| row.get("display_name"))
                .and_then(Value::as_str)
                .map(str::to_string);
            let context = row
                .get("context_length")
                .or_else(|| row.get("context_window"))
                .and_then(Value::as_u64);
            let tools = row
                .get("supported_parameters")
                .and_then(Value::as_array)
                .map(|values| values.iter().any(|v| v.as_str() == Some("tools")));
            let images = row
                .pointer("/architecture/input_modalities")
                .and_then(Value::as_array)
                .map(|values| values.iter().any(|v| v.as_str() == Some("image")));
            Some(remote_entry(id, name, context, tools, images))
        })
        .collect())
}

fn remote_entry(
    id: &str,
    display_name: Option<String>,
    context_window: Option<u64>,
    supports_tools: Option<bool>,
    supports_images: Option<bool>,
) -> ModelCatalogEntry {
    ModelCatalogEntry {
        provider_alias: String::new(),
        id: id.to_string(),
        display_name,
        context_window,
        supports_tools,
        supports_images,
        source: CatalogSource::Remote,
    }
}

async fn get_json(
    transport: &CatalogTransport,
    url: impl reqwest::IntoUrl,
) -> anyhow::Result<Value> {
    let response = transport.http.get(url).send().await?.error_for_status()?;
    if response
        .content_length()
        .is_some_and(|length| length > MAX_BODY_BYTES as u64)
    {
        anyhow::bail!(
            "catalog response exceeds {} MiB",
            MAX_BODY_BYTES / 1024 / 1024
        )
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if body.len().saturating_add(chunk.len()) > MAX_BODY_BYTES {
            anyhow::bail!(
                "catalog response exceeds {} MiB",
                MAX_BODY_BYTES / 1024 / 1024
            )
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_openrouter_metadata() {
        let json = serde_json::json!({"data": [{
            "id": "anthropic/claude-test",
            "name": "Claude Test",
            "context_length": 200000,
            "supported_parameters": ["tools"],
            "architecture": {"input_modalities": ["text", "image"]}
        }]});
        let rows = parse_openai_models(&json).unwrap();
        assert_eq!(rows[0].id, "anthropic/claude-test");
        assert_eq!(rows[0].context_window, Some(200000));
        assert_eq!(rows[0].supports_tools, Some(true));
        assert_eq!(rows[0].supports_images, Some(true));
    }

    #[test]
    fn parses_anthropic_models() {
        let rows = parse_anthropic_models(&serde_json::json!({
            "data": [{"id": "claude-test", "display_name": "Claude Test"}]
        }))
        .unwrap();
        assert_eq!(rows[0].id, "claude-test");
        assert_eq!(rows[0].display_name.as_deref(), Some("Claude Test"));
    }

    #[test]
    fn parses_only_generation_capable_gemini_models() {
        let rows = parse_gemini_models(&serde_json::json!({"models": [
            {"name": "models/gemini-test", "displayName": "Gemini Test", "inputTokenLimit": 1000000,
             "supportedGenerationMethods": ["generateContent"]},
            {"name": "models/embedding-test", "supportedGenerationMethods": ["embedContent"]}
        ]}))
        .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "gemini-test");
        assert_eq!(rows[0].context_window, Some(1_000_000));
    }

    #[test]
    fn parses_ollama_tags() {
        let rows = parse_ollama_models(&serde_json::json!({
            "models": [{"name": "qwen3:8b"}, {"name": "llama3.2:latest"}]
        }))
        .unwrap();
        assert_eq!(
            rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
            ["qwen3:8b", "llama3.2:latest"]
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
}
