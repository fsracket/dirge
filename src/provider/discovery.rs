//! Narrow provider model-discovery capability and response normalization.

use futures::StreamExt;
use serde_json::Value;

use super::backend::{DiscoverySurface, ResolvedDiscovery};

const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;
const MAX_PAGES: usize = 10;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredModel {
    pub id: String,
    pub display_name: Option<String>,
    pub context_window: Option<u64>,
    pub supports_tools: Option<bool>,
    pub supports_images: Option<bool>,
}

struct DiscoveryTransport {
    base_url: String,
    http: reqwest::Client,
}

pub struct OpenAiCompatibleDiscoveryClient(DiscoveryTransport);
pub struct ChatGptCodexDiscoveryClient(DiscoveryTransport);
pub struct AnthropicDiscoveryClient(DiscoveryTransport);
pub struct GeminiDiscoveryClient(DiscoveryTransport);
pub struct OllamaDiscoveryClient(DiscoveryTransport);

/// Explicit enum dispatch keeps provider compatibility choices reviewable and
/// prevents provider-specific JSON from reaching catalog aggregation or UI.
pub enum ModelDiscoveryClient {
    OpenAiCompatible(OpenAiCompatibleDiscoveryClient),
    ChatGptCodex(ChatGptCodexDiscoveryClient),
    Anthropic(AnthropicDiscoveryClient),
    Gemini(GeminiDiscoveryClient),
    Ollama(OllamaDiscoveryClient),
    Unavailable,
}

impl ModelDiscoveryClient {
    pub(crate) fn from_resolved(resolved: ResolvedDiscovery) -> anyhow::Result<Self> {
        tracing::debug!(
            target: "dirge::provider",
            alias = %resolved.alias,
            auth = ?resolved.auth_mode,
            credential = ?resolved.credential_shape,
            surface = ?resolved.discovery_surface,
            "constructing model-discovery capability",
        );
        if resolved.discovery_surface == DiscoverySurface::Unavailable {
            return Ok(Self::Unavailable);
        }
        let transport = DiscoveryTransport {
            base_url: resolved.discovery_base_url,
            http: reqwest::Client::builder()
                .default_headers(resolved.headers)
                .timeout(std::time::Duration::from_secs(8))
                .build()?,
        };
        Ok(match resolved.discovery_surface {
            DiscoverySurface::OpenAiModelsCompatible => {
                Self::OpenAiCompatible(OpenAiCompatibleDiscoveryClient(transport))
            }
            DiscoverySurface::ChatGptCodexModels => {
                Self::ChatGptCodex(ChatGptCodexDiscoveryClient(transport))
            }
            DiscoverySurface::AnthropicModels => {
                Self::Anthropic(AnthropicDiscoveryClient(transport))
            }
            DiscoverySurface::GeminiModels => Self::Gemini(GeminiDiscoveryClient(transport)),
            DiscoverySurface::OllamaTags => Self::Ollama(OllamaDiscoveryClient(transport)),
            DiscoverySurface::Unavailable => Self::Unavailable,
        })
    }

    pub async fn list_models(&self) -> anyhow::Result<Vec<DiscoveredModel>> {
        match self {
            Self::OpenAiCompatible(client) => fetch_openai_compatible(&client.0).await,
            Self::ChatGptCodex(client) => fetch_chatgpt_codex(&client.0).await,
            Self::Anthropic(client) => fetch_anthropic(&client.0).await,
            Self::Gemini(client) => fetch_gemini(&client.0).await,
            Self::Ollama(client) => fetch_ollama(&client.0).await,
            Self::Unavailable => Ok(Vec::new()),
        }
    }
}

async fn fetch_openai_compatible(
    transport: &DiscoveryTransport,
) -> anyhow::Result<Vec<DiscoveredModel>> {
    let json = get_json(transport, format!("{}/models", transport.base_url)).await?;
    parse_openai_models(&json)
}

async fn fetch_chatgpt_codex(
    transport: &DiscoveryTransport,
) -> anyhow::Result<Vec<DiscoveredModel>> {
    let json = get_json(transport, chatgpt_codex_models_url(&transport.base_url)?).await?;
    parse_chatgpt_codex_models(&json)
}

fn chatgpt_codex_models_url(base_url: &str) -> anyhow::Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(&format!("{base_url}/models"))?;
    url.query_pairs_mut()
        .append_pair("client_version", env!("CARGO_PKG_VERSION"));
    Ok(url)
}

async fn fetch_ollama(transport: &DiscoveryTransport) -> anyhow::Result<Vec<DiscoveredModel>> {
    let base = transport.base_url.trim_end_matches("/api");
    let json = get_json(transport, format!("{base}/api/tags")).await?;
    parse_ollama_models(&json)
}

fn parse_ollama_models(json: &Value) -> anyhow::Result<Vec<DiscoveredModel>> {
    let rows = json
        .get("models")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("response has no models array"))?;
    Ok(rows
        .iter()
        .filter_map(|row| row.get("name").and_then(Value::as_str))
        .map(|id| discovered(id, None, None, None, None))
        .collect())
}

async fn fetch_anthropic(transport: &DiscoveryTransport) -> anyhow::Result<Vec<DiscoveredModel>> {
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

fn parse_anthropic_models(json: &Value) -> anyhow::Result<Vec<DiscoveredModel>> {
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
            Some(discovered(id, name, None, None, None))
        })
        .collect())
}

async fn fetch_gemini(transport: &DiscoveryTransport) -> anyhow::Result<Vec<DiscoveredModel>> {
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

fn parse_gemini_models(json: &Value) -> anyhow::Result<Vec<DiscoveredModel>> {
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
                .any(|method| method.as_str() == Some("generateContent"))
            {
                return None;
            }
            let id = row.get("name")?.as_str()?.trim_start_matches("models/");
            let name = row
                .get("displayName")
                .and_then(Value::as_str)
                .map(str::to_string);
            let context = row.get("inputTokenLimit").and_then(Value::as_u64);
            Some(discovered(id, name, context, None, None))
        })
        .collect())
}

fn parse_openai_models(json: &Value) -> anyhow::Result<Vec<DiscoveredModel>> {
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
                .map(|values| values.iter().any(|value| value.as_str() == Some("tools")));
            let images = row
                .pointer("/architecture/input_modalities")
                .and_then(Value::as_array)
                .map(|values| values.iter().any(|value| value.as_str() == Some("image")));
            Some(discovered(id, name, context, tools, images))
        })
        .collect())
}

fn parse_chatgpt_codex_models(json: &Value) -> anyhow::Result<Vec<DiscoveredModel>> {
    let rows = json
        .get("models")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("response has no models array"))?;
    Ok(rows
        .iter()
        .filter(|row| row.get("visibility").and_then(Value::as_str) == Some("list"))
        .filter_map(|row| {
            let id = row.get("slug")?.as_str()?;
            let name = row
                .get("display_name")
                .and_then(Value::as_str)
                .map(str::to_string);
            let context = row.get("context_window").and_then(Value::as_u64);
            let tools = row
                .get("shell_type")
                .and_then(Value::as_str)
                .map(|shell_type| shell_type != "disabled");
            let images = row
                .get("input_modalities")
                .and_then(Value::as_array)
                .map(|values| values.iter().any(|value| value.as_str() == Some("image")));
            Some(discovered(id, name, context, tools, images))
        })
        .collect())
}

fn discovered(
    id: &str,
    display_name: Option<String>,
    context_window: Option<u64>,
    supports_tools: Option<bool>,
    supports_images: Option<bool>,
) -> DiscoveredModel {
    DiscoveredModel {
        id: id.to_string(),
        display_name,
        context_window,
        supports_tools,
        supports_images,
    }
}

async fn get_json(
    transport: &DiscoveryTransport,
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
        let rows = parse_openai_models(&serde_json::json!({"data": [{
            "id": "anthropic/claude-test",
            "name": "Claude Test",
            "context_length": 200000,
            "supported_parameters": ["tools"],
            "architecture": {"input_modalities": ["text", "image"]}
        }]}))
        .unwrap();
        assert_eq!(rows[0].id, "anthropic/claude-test");
        assert_eq!(rows[0].context_window, Some(200000));
        assert_eq!(rows[0].supports_tools, Some(true));
        assert_eq!(rows[0].supports_images, Some(true));
    }

    #[test]
    fn chatgpt_codex_endpoint_includes_dirge_version() {
        let url = chatgpt_codex_models_url("https://chatgpt.com/backend-api/codex").unwrap();
        assert_eq!(
            url.as_str(),
            format!(
                "https://chatgpt.com/backend-api/codex/models?client_version={}",
                env!("CARGO_PKG_VERSION")
            )
        );
    }

    #[test]
    fn parses_chatgpt_codex_fixture() {
        let fixture: Value =
            serde_json::from_str(include_str!("fixtures/chatgpt_codex_models.json")).unwrap();
        let rows = parse_chatgpt_codex_models(&fixture).unwrap();
        assert_eq!(rows.len(), 2, "hidden catalog rows stay out of the picker");
        assert_eq!(rows[0].id, "gpt-fixture-codex");
        assert_eq!(rows[0].display_name.as_deref(), Some("GPT Fixture Codex"));
        assert_eq!(rows[0].context_window, Some(272_000));
        assert_eq!(rows[0].supports_tools, Some(true));
        assert_eq!(rows[0].supports_images, Some(true));
        assert_eq!(rows[1].supports_tools, Some(false));
        assert_eq!(rows[1].supports_images, Some(false));
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
}
