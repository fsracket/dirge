//! Provider-level resolution shared by inference and model discovery.
//!
//! Backend construction resolves only stable, non-secret configuration. Each
//! capability method then resolves current credentials, authentication,
//! headers, URL policy, and wire surface immediately before constructing the
//! concrete client.

use std::collections::HashMap;
use std::fmt;

use crate::auth::store::OpenAiOAuthCredential;
use crate::config::{ProviderAuth, ProviderEntry};

use super::auth::{ProviderAuthHeaders, resolve_auth_headers};
use super::{AnyClient, ProviderKind, resolve_api_key_from};

pub(crate) const CHATGPT_CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialKind {
    ApiKey,
    ChatGptBearer,
    OpenAiOAuth,
    AnthropicOAuth,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CredentialShape {
    pub kind: CredentialKind,
    pub present: bool,
    pub refreshable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InferenceSurface {
    ChatCompletionsCompatible,
    ResponsesCompatible,
    AnthropicMessages,
    GeminiGenerateContent,
    Ollama,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscoverySurface {
    OpenAiModelsCompatible,
    AnthropicModels,
    GeminiModels,
    OllamaTags,
    Unavailable,
}

/// Stable, non-secret recipe for obtaining credentials at capability
/// construction time. Literal keys and interpolated values deliberately do
/// not appear here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialPolicy {
    pub api_key_env: Option<String>,
    pub allow_openai_oauth: bool,
}

/// Validated provider identity and stable policy. Its `Debug` output is safe:
/// it records only header presence and credential-source names, never values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedProviderDefinition {
    pub alias: String,
    pub kind: ProviderKind,
    pub base_url: Option<String>,
    pub auth: Option<ProviderAuth>,
    pub credential_policy: CredentialPolicy,
    pub has_configured_headers: bool,
}

impl ResolvedProviderDefinition {
    pub(crate) fn new(
        alias: &str,
        kind: ProviderKind,
        base_url: Option<String>,
        api_key_env: Option<String>,
        auth: Option<ProviderAuth>,
        has_configured_headers: bool,
    ) -> Self {
        let allow_openai_oauth = alias.eq_ignore_ascii_case("openai")
            && kind == ProviderKind::OpenAI
            && base_url.is_none();
        Self {
            alias: alias.to_string(),
            kind,
            base_url,
            auth,
            credential_policy: CredentialPolicy {
                api_key_env,
                allow_openai_oauth,
            },
            has_configured_headers,
        }
    }

    /// Compatibility accessor retained for callers that inspect stable
    /// provider policy. The value is an environment-variable name, not a key.
    pub fn api_key_env(&self) -> Option<&str> {
        self.credential_policy.api_key_env.as_deref()
    }
}

/// Provider-centred runtime object. The entry may contain configured secrets,
/// so this type intentionally has no `Debug` implementation.
pub struct ProviderBackend {
    definition: ResolvedProviderDefinition,
    entry: ProviderEntry,
    /// Preserve the pre-backend boundary: only user-configured providers may
    /// contribute arbitrary request headers. Plugin entries still contribute
    /// provider metadata and credential policy, but their headers are ignored.
    include_entry_headers: bool,
}

impl ProviderBackend {
    pub fn new(
        provider_name: &str,
        providers: &HashMap<String, ProviderEntry>,
        default_auth: Option<ProviderAuth>,
    ) -> anyhow::Result<Self> {
        let mut definition = super::resolve_provider_info(provider_name, providers).ok_or_else(|| {
            anyhow::anyhow!(
                "Unknown provider: {}. Supported providers: openrouter, openai, anthropic, gemini, deepseek, glm, cerebras, opencode, ollama, custom",
                provider_name
            )
        })?;
        definition.auth = definition.auth.or(default_auth);
        let lower = provider_name.to_ascii_lowercase();
        let include_entry_headers = providers
            .get(provider_name)
            .or_else(|| providers.get(&lower))
            .is_some();
        let entry =
            super::resolve::resolve_provider_entry(provider_name, providers).unwrap_or_default();
        Ok(Self {
            definition,
            entry,
            include_entry_headers,
        })
    }

    #[allow(dead_code)]
    pub fn descriptor(&self) -> &ResolvedProviderDefinition {
        &self.definition
    }

    pub fn inference_client(&self, api_key: Option<&str>) -> anyhow::Result<AnyClient> {
        self.inference_client_with(
            api_key,
            |name| std::env::var(name).ok(),
            super::client::load_fresh_openai_oauth,
            None,
        )
    }

    pub fn model_discovery(&self) -> anyhow::Result<super::discovery::ModelDiscoveryClient> {
        let plan = self.resolve_discovery_with(
            |name| std::env::var(name).ok(),
            super::client::load_fresh_openai_oauth,
            None,
        )?;
        super::discovery::ModelDiscoveryClient::from_resolved(plan)
    }

    pub(crate) fn inference_client_with<F, G>(
        &self,
        api_key: Option<&str>,
        env: F,
        load_openai_oauth: G,
        resolved_auth_headers: Option<ProviderAuthHeaders>,
    ) -> anyhow::Result<AnyClient>
    where
        F: Fn(&str) -> Option<String>,
        G: FnOnce() -> anyhow::Result<Option<OpenAiOAuthCredential>>,
    {
        let plan =
            self.resolve_inference_with(api_key, env, load_openai_oauth, resolved_auth_headers)?;
        super::client::build_inference_client(plan)
    }

    fn resolve_inference_with<F, G>(
        &self,
        api_key: Option<&str>,
        env: F,
        load_openai_oauth: G,
        resolved_auth_headers: Option<ProviderAuthHeaders>,
    ) -> anyhow::Result<ResolvedInference>
    where
        F: Fn(&str) -> Option<String>,
        G: FnOnce() -> anyhow::Result<Option<OpenAiOAuthCredential>>,
    {
        let common = self.resolve_common(api_key, env, load_openai_oauth, resolved_auth_headers)?;
        let inference_surface =
            inference_surface(self.definition.kind, common.auth_mode, &common.credential);
        Ok(ResolvedInference {
            alias: self.definition.alias.clone(),
            kind: self.definition.kind,
            auth_mode: common.auth_mode,
            credential_shape: common.credential.shape(common.auth_mode),
            inference_surface,
            inference_base_url: common.base_url,
            credential: common.credential,
            headers: common.inference_headers,
            auth_headers: common.auth_headers,
        })
    }

    fn resolve_discovery_with<F, G>(
        &self,
        env: F,
        load_openai_oauth: G,
        resolved_auth_headers: Option<ProviderAuthHeaders>,
    ) -> anyhow::Result<ResolvedDiscovery>
    where
        F: Fn(&str) -> Option<String>,
        G: FnOnce() -> anyhow::Result<Option<OpenAiOAuthCredential>>,
    {
        let common = self.resolve_common(None, env, load_openai_oauth, resolved_auth_headers)?;
        let discovery_surface = discovery_surface(self.definition.kind);
        let base_url = common
            .base_url
            .unwrap_or_else(|| default_discovery_base_url(self.definition.kind).to_string());
        if base_url.is_empty() {
            anyhow::bail!(
                "provider '{}' has no catalog base URL",
                self.definition.alias
            )
        }
        let headers = discovery_headers(
            common.inference_headers,
            self.definition.kind,
            common.auth_mode,
            &common.credential,
            common.auth_headers.as_ref(),
        )?;
        Ok(ResolvedDiscovery {
            alias: self.definition.alias.clone(),
            auth_mode: common.auth_mode,
            credential_shape: common.credential.shape(common.auth_mode),
            discovery_surface,
            discovery_base_url: base_url.trim_end_matches('/').to_string(),
            headers,
        })
    }

    fn resolve_common<F, G>(
        &self,
        api_key: Option<&str>,
        env: F,
        load_openai_oauth: G,
        resolved_auth_headers: Option<ProviderAuthHeaders>,
    ) -> anyhow::Result<ResolvedCommon>
    where
        F: Fn(&str) -> Option<String>,
        G: FnOnce() -> anyhow::Result<Option<OpenAiOAuthCredential>>,
    {
        let mut inference_headers = if self.include_entry_headers {
            self.entry.resolved_headers()?
        } else {
            rig::http_client::HeaderMap::new()
        };
        let anthropic_oauth_env_present =
            env("ANTHROPIC_OAUTH_TOKEN").is_some_and(|value| !value.trim().is_empty());
        let anthropic_oauth_file_present = super::anthropic_oauth::credentials_file_path().exists();
        let anthropic_oauth_present = self.definition.kind == ProviderKind::Anthropic
            && (anthropic_oauth_env_present || anthropic_oauth_file_present);
        let auth_mode = effective_auth(
            self.definition.auth,
            self.definition.kind,
            anthropic_oauth_present,
        );
        validate_auth_kind(auth_mode, self.definition.kind, &self.definition.alias)?;
        let auth_headers = match (auth_mode, resolved_auth_headers) {
            (ProviderAuth::ChatGpt | ProviderAuth::Anthropic, Some(headers)) => Some(headers),
            _ => resolve_auth_headers(auth_mode)?,
        };
        let credential = if let Some(headers) = auth_headers.as_ref() {
            ProviderCredential::Bearer {
                token: headers.bearer_token.clone(),
                refreshable: match auth_mode {
                    ProviderAuth::ChatGpt => headers.chatgpt_bearer_is_dirge_oauth,
                    ProviderAuth::Anthropic => {
                        !anthropic_oauth_env_present && anthropic_oauth_file_present
                    }
                    ProviderAuth::ApiKey => false,
                },
            }
        } else {
            let configured_literal = match self.entry.resolved_api_key() {
                Some(Ok(value)) => Some(value),
                Some(Err(missing)) => {
                    tracing::error!(
                        target: "dirge::provider",
                        alias = %self.definition.alias,
                        "provider references env var ${{{missing}}} via api_key but it is unset",
                    );
                    eprintln!(
                        "error: provider '{}' references env var ${{{missing}}} via api_key but it is unset",
                        self.definition.alias
                    );
                    None
                }
                None => None,
            };
            resolve_provider_credential(
                self.definition.credential_policy.allow_openai_oauth,
                self.definition.kind,
                configured_literal.as_deref(),
                self.definition.credential_policy.api_key_env.as_deref(),
                api_key,
                &env,
                load_openai_oauth,
            )?
        };
        let uses_openai_oauth = credential.is_openai_oauth();
        let is_chatgpt_auth = auth_mode == ProviderAuth::ChatGpt;
        let base_url = match self.definition.kind {
            ProviderKind::OpenAI if uses_openai_oauth => Some(CHATGPT_CODEX_BASE_URL.to_string()),
            ProviderKind::OpenAI if is_chatgpt_auth => self
                .definition
                .base_url
                .clone()
                .or_else(|| Some(CHATGPT_CODEX_BASE_URL.to_string())),
            _ => resolve_provider_base_url(
                self.definition.kind,
                self.definition.base_url.clone(),
                &env,
            )?,
        };
        if (is_chatgpt_auth || uses_openai_oauth || auth_mode == ProviderAuth::Anthropic)
            && let Some(url) = base_url.as_deref()
            && !url.starts_with("https://")
        {
            anyhow::bail!(
                "OAuth login auth requires an https base URL, but got `{url}`. The OAuth bearer token is too sensitive to send over http:// — `allow_insecure` is ignored here."
            );
        }
        if let Some(account_id) = auth_headers
            .as_ref()
            .and_then(|headers| headers.chatgpt_account_id.as_deref())
            .filter(|id| !id.trim().is_empty())
        {
            inference_headers.insert(
                http::HeaderName::from_static("chatgpt-account-id"),
                header_value(account_id)?,
            );
        }
        Ok(ResolvedCommon {
            auth_mode,
            credential,
            base_url,
            inference_headers,
            auth_headers,
        })
    }
}

pub struct ResolvedInference {
    pub alias: String,
    pub kind: ProviderKind,
    pub auth_mode: ProviderAuth,
    pub credential_shape: CredentialShape,
    pub inference_surface: InferenceSurface,
    pub inference_base_url: Option<String>,
    pub(crate) credential: ProviderCredential,
    pub(crate) headers: rig::http_client::HeaderMap,
    pub(crate) auth_headers: Option<ProviderAuthHeaders>,
}

pub struct ResolvedDiscovery {
    pub alias: String,
    pub auth_mode: ProviderAuth,
    pub credential_shape: CredentialShape,
    pub discovery_surface: DiscoverySurface,
    pub discovery_base_url: String,
    pub(crate) headers: rig::http_client::HeaderMap,
}

struct ResolvedCommon {
    auth_mode: ProviderAuth,
    credential: ProviderCredential,
    base_url: Option<String>,
    inference_headers: rig::http_client::HeaderMap,
    auth_headers: Option<ProviderAuthHeaders>,
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) enum ProviderCredential {
    ApiKey(String),
    Bearer {
        token: String,
        refreshable: bool,
    },
    OpenAiOAuth {
        access_token: String,
        account_id: Option<String>,
    },
}

impl fmt::Debug for ProviderCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ApiKey(_) => f.debug_tuple("ApiKey").field(&"[REDACTED]").finish(),
            Self::Bearer { refreshable, .. } => f
                .debug_struct("Bearer")
                .field("token", &"[REDACTED]")
                .field("refreshable", refreshable)
                .finish(),
            Self::OpenAiOAuth { account_id, .. } => f
                .debug_struct("OpenAiOAuth")
                .field("access_token", &"[REDACTED]")
                .field("account_id", account_id)
                .finish(),
        }
    }
}

impl ProviderCredential {
    pub(crate) fn secret(&self) -> &str {
        match self {
            Self::ApiKey(secret) | Self::Bearer { token: secret, .. } => secret,
            Self::OpenAiOAuth { access_token, .. } => access_token,
        }
    }

    pub(crate) fn into_secret(self) -> String {
        match self {
            Self::ApiKey(secret) | Self::Bearer { token: secret, .. } => secret,
            Self::OpenAiOAuth { access_token, .. } => access_token,
        }
    }

    pub(crate) fn is_openai_oauth(&self) -> bool {
        matches!(self, Self::OpenAiOAuth { .. })
    }

    pub(crate) fn openai_oauth_account_id(&self) -> Option<&str> {
        match self {
            Self::OpenAiOAuth { account_id, .. } => account_id.as_deref(),
            _ => None,
        }
    }

    fn shape(&self, auth_mode: ProviderAuth) -> CredentialShape {
        match self {
            Self::ApiKey(secret) => CredentialShape {
                kind: CredentialKind::ApiKey,
                present: !secret.is_empty(),
                refreshable: false,
            },
            Self::Bearer { token, refreshable } => CredentialShape {
                kind: if auth_mode == ProviderAuth::Anthropic {
                    CredentialKind::AnthropicOAuth
                } else {
                    CredentialKind::ChatGptBearer
                },
                present: !token.is_empty(),
                refreshable: *refreshable,
            },
            Self::OpenAiOAuth { access_token, .. } => CredentialShape {
                kind: CredentialKind::OpenAiOAuth,
                present: !access_token.is_empty(),
                refreshable: true,
            },
        }
    }
}

pub(crate) fn effective_auth(
    explicit: Option<ProviderAuth>,
    kind: ProviderKind,
    anthropic_oauth_present: bool,
) -> ProviderAuth {
    match explicit {
        Some(auth) => auth,
        None if kind == ProviderKind::Anthropic && anthropic_oauth_present => {
            ProviderAuth::Anthropic
        }
        None => ProviderAuth::ApiKey,
    }
}

fn validate_auth_kind(
    auth: ProviderAuth,
    kind: ProviderKind,
    provider_name: &str,
) -> anyhow::Result<()> {
    if auth == ProviderAuth::ChatGpt && kind != ProviderKind::OpenAI {
        anyhow::bail!(
            "ChatGPT (Codex) auth is only supported for the `openai` provider, not `{provider_name}`. Set `auth: chatgpt` only on your openai provider (or use an API key for `{provider_name}`)."
        )
    }
    if auth == ProviderAuth::Anthropic && kind != ProviderKind::Anthropic {
        anyhow::bail!(
            "Anthropic OAuth is only supported for the `anthropic` provider, not `{provider_name}`. Set `auth: anthropic` only on your anthropic provider (or use an API key for `{provider_name}`)."
        )
    }
    Ok(())
}

pub(crate) fn resolve_provider_credential<F, G>(
    allow_openai_oauth: bool,
    kind: ProviderKind,
    api_key_literal: Option<&str>,
    api_key_env: Option<&str>,
    cli_key: Option<&str>,
    env: F,
    load_openai_oauth: G,
) -> anyhow::Result<ProviderCredential>
where
    F: Fn(&str) -> Option<String>,
    G: FnOnce() -> anyhow::Result<Option<OpenAiOAuthCredential>>,
{
    let mut openai_oauth_error = None;
    if kind == ProviderKind::OpenAI && allow_openai_oauth {
        match load_openai_oauth() {
            Ok(Some(credential)) => {
                return Ok(ProviderCredential::OpenAiOAuth {
                    access_token: credential.access_token().to_string(),
                    account_id: credential.account_id().map(str::to_string),
                });
            }
            Ok(None) => {}
            Err(err) => openai_oauth_error = Some(err),
        }
    }
    if let Some(key) = cli_key.filter(|key| !key.is_empty()) {
        return Ok(ProviderCredential::ApiKey(key.to_string()));
    }
    if let Some(key) = api_key_literal.filter(|key| !key.is_empty()) {
        return Ok(ProviderCredential::ApiKey(key.to_string()));
    }
    resolve_api_key_from(kind, api_key_env, None, env)
        .map(ProviderCredential::ApiKey)
        .map_err(|err| {
            if let Some(openai_oauth_error) = openai_oauth_error {
                return openai_oauth_error;
            }
            if kind == ProviderKind::OpenAI && allow_openai_oauth {
                anyhow::anyhow!(
                    "{err} You can also run `dirge auth openai` to use a stored OpenAI OAuth login."
                )
            } else {
                err
            }
        })
}

pub(crate) fn resolve_provider_base_url(
    kind: ProviderKind,
    config_url: Option<String>,
    env: impl Fn(&str) -> Option<String>,
) -> anyhow::Result<Option<String>> {
    if let Some(configured) = config_url {
        return Ok(Some(configured));
    }
    let (env_var, default): (Option<&str>, Option<&str>) = match kind {
        ProviderKind::DeepSeek => (
            Some("DEEPSEEK_BASE_URL"),
            Some("https://api.deepseek.com/v1"),
        ),
        ProviderKind::Glm => (
            Some("GLM_BASE_URL"),
            Some("https://open.bigmodel.cn/api/coding/paas/v4"),
        ),
        ProviderKind::Cerebras => (None, Some("https://api.cerebras.ai/v1")),
        ProviderKind::Custom => (Some("CUSTOM_BASE_URL"), None),
        _ => (None, None),
    };
    if let Some(var) = env_var
        && let Some(url) = env(var).filter(|url| !url.is_empty())
    {
        if !url.starts_with("https://") {
            anyhow::bail!(
                "${var} is `{url}`, but only https:// base URLs are accepted from the environment (an http:// endpoint sends the API key in plaintext). Use https://, or configure a `custom` provider with `allow_insecure: true` for a local-only http endpoint."
            );
        }
        return Ok(Some(url));
    }
    Ok(default.map(str::to_string))
}

fn inference_surface(
    kind: ProviderKind,
    auth: ProviderAuth,
    credential: &ProviderCredential,
) -> InferenceSurface {
    match kind {
        ProviderKind::OpenAI if auth == ProviderAuth::ChatGpt || credential.is_openai_oauth() => {
            InferenceSurface::ResponsesCompatible
        }
        ProviderKind::Anthropic => InferenceSurface::AnthropicMessages,
        ProviderKind::Gemini => InferenceSurface::GeminiGenerateContent,
        ProviderKind::Ollama => InferenceSurface::Ollama,
        _ => InferenceSurface::ChatCompletionsCompatible,
    }
}

fn discovery_surface(kind: ProviderKind) -> DiscoverySurface {
    match kind {
        ProviderKind::Anthropic => DiscoverySurface::AnthropicModels,
        ProviderKind::Gemini => DiscoverySurface::GeminiModels,
        ProviderKind::Ollama => DiscoverySurface::OllamaTags,
        ProviderKind::OpenRouter
        | ProviderKind::OpenAI
        | ProviderKind::DeepSeek
        | ProviderKind::Glm
        | ProviderKind::Cerebras
        | ProviderKind::OpenCode
        | ProviderKind::Custom => DiscoverySurface::OpenAiModelsCompatible,
    }
}

fn default_discovery_base_url(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::OpenRouter => "https://openrouter.ai/api/v1",
        ProviderKind::OpenAI => "https://api.openai.com/v1",
        ProviderKind::Anthropic => "https://api.anthropic.com",
        ProviderKind::Gemini => "https://generativelanguage.googleapis.com",
        ProviderKind::DeepSeek => "https://api.deepseek.com/v1",
        ProviderKind::Glm => "https://open.bigmodel.cn/api/coding/paas/v4",
        ProviderKind::Cerebras => "https://api.cerebras.ai/v1",
        ProviderKind::OpenCode => "https://opencode.ai/zen/v1",
        ProviderKind::Ollama => "http://localhost:11434",
        ProviderKind::Custom => "",
    }
}

fn discovery_headers(
    mut headers: rig::http_client::HeaderMap,
    kind: ProviderKind,
    auth: ProviderAuth,
    credential: &ProviderCredential,
    auth_headers: Option<&ProviderAuthHeaders>,
) -> anyhow::Result<rig::http_client::HeaderMap> {
    let secret = credential.secret();
    match kind {
        ProviderKind::Anthropic => {
            if auth == ProviderAuth::Anthropic {
                headers.insert(
                    http::HeaderName::from_static("authorization"),
                    header_value(&format!("Bearer {secret}"))?,
                );
                headers.insert(
                    http::HeaderName::from_static("anthropic-beta"),
                    http::HeaderValue::from_static("oauth-2025-04-20"),
                );
            } else {
                headers.insert(
                    http::HeaderName::from_static("x-api-key"),
                    header_value(secret)?,
                );
            }
            headers.insert(
                http::HeaderName::from_static("anthropic-version"),
                http::HeaderValue::from_static("2023-06-01"),
            );
        }
        ProviderKind::Gemini => {
            headers.insert(
                http::HeaderName::from_static("x-goog-api-key"),
                header_value(secret)?,
            );
        }
        ProviderKind::Ollama if secret.is_empty() => {}
        _ => {
            headers.insert(
                http::HeaderName::from_static("authorization"),
                header_value(&format!("Bearer {secret}"))?,
            );
        }
    }
    if let Some(account_id) = auth_headers
        .and_then(|resolved| resolved.chatgpt_account_id.as_deref())
        .filter(|id| !id.trim().is_empty())
    {
        headers.insert(
            http::HeaderName::from_static("chatgpt-account-id"),
            header_value(account_id)?,
        );
    }
    headers.insert(
        http::HeaderName::from_static("accept"),
        http::HeaderValue::from_static("application/json"),
    );
    Ok(headers)
}

fn header_value(value: &str) -> anyhow::Result<http::HeaderValue> {
    http::HeaderValue::from_str(value)
        .map_err(|error| anyhow::anyhow!("invalid resolved authentication header: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn stable_descriptor_is_secret_free_and_debug_safe() {
        let providers = HashMap::from([(
            "proxy".to_string(),
            ProviderEntry {
                provider_type: Some("openai".to_string()),
                base_url: Some("https://proxy.example/v1".to_string()),
                api_key: Some("SUPER-SECRET".to_string()),
                headers: Some(HashMap::from([(
                    "Authorization".to_string(),
                    "HEADER-SECRET".to_string(),
                )])),
                ..Default::default()
            },
        )]);
        let backend = ProviderBackend::new("proxy", &providers, None).unwrap();
        let rendered = format!("{:?}", backend.descriptor());
        assert!(!rendered.contains("SUPER-SECRET"));
        assert!(!rendered.contains("HEADER-SECRET"));
        assert!(backend.descriptor().has_configured_headers);
        let resolved = backend
            .resolve_inference_with(None, no_env, || Ok(None), None)
            .unwrap();
        assert_eq!(
            resolved
                .headers
                .get("authorization")
                .and_then(|value| value.to_str().ok()),
            Some("HEADER-SECRET")
        );
    }

    #[test]
    fn plugin_entry_headers_remain_outside_request_policy() {
        let definition = ResolvedProviderDefinition::new(
            "plugin-proxy",
            ProviderKind::OpenAI,
            Some("https://plugin.example/v1".to_string()),
            None,
            None,
            false,
        );
        let backend = ProviderBackend {
            definition,
            entry: ProviderEntry {
                headers: Some(HashMap::from([(
                    "x-plugin-secret".to_string(),
                    "must-not-be-sent".to_string(),
                )])),
                ..Default::default()
            },
            include_entry_headers: false,
        };

        let resolved = backend
            .resolve_inference_with(Some("test-key"), no_env, || Ok(None), None)
            .unwrap();
        assert!(resolved.headers.get("x-plugin-secret").is_none());
    }

    #[test]
    fn capability_surface_matrix_preserves_existing_dispatch() {
        let cases = [
            (
                "openrouter",
                InferenceSurface::ChatCompletionsCompatible,
                DiscoverySurface::OpenAiModelsCompatible,
            ),
            (
                "anthropic",
                InferenceSurface::AnthropicMessages,
                DiscoverySurface::AnthropicModels,
            ),
            (
                "gemini",
                InferenceSurface::GeminiGenerateContent,
                DiscoverySurface::GeminiModels,
            ),
            (
                "ollama",
                InferenceSurface::Ollama,
                DiscoverySurface::OllamaTags,
            ),
            (
                "openai",
                InferenceSurface::ChatCompletionsCompatible,
                DiscoverySurface::OpenAiModelsCompatible,
            ),
            (
                "deepseek",
                InferenceSurface::ChatCompletionsCompatible,
                DiscoverySurface::OpenAiModelsCompatible,
            ),
            (
                "glm",
                InferenceSurface::ChatCompletionsCompatible,
                DiscoverySurface::OpenAiModelsCompatible,
            ),
            (
                "cerebras",
                InferenceSurface::ChatCompletionsCompatible,
                DiscoverySurface::OpenAiModelsCompatible,
            ),
            (
                "opencode",
                InferenceSurface::ChatCompletionsCompatible,
                DiscoverySurface::OpenAiModelsCompatible,
            ),
            (
                "custom",
                InferenceSurface::ChatCompletionsCompatible,
                DiscoverySurface::OpenAiModelsCompatible,
            ),
        ];
        for (alias, inference, discovery) in cases {
            // Explicit API-key auth makes the Anthropic row independent of a
            // developer's real stored OAuth credential and prevents refresh
            // or network behavior inside this dispatch-only unit test.
            let providers = if alias == "anthropic" {
                HashMap::from([(
                    alias.to_string(),
                    ProviderEntry {
                        auth: Some(ProviderAuth::ApiKey),
                        ..Default::default()
                    },
                )])
            } else {
                HashMap::new()
            };
            let backend = ProviderBackend::new(alias, &providers, None).unwrap();
            let resolved = backend
                .resolve_inference_with(
                    None,
                    |name| match name {
                        "CUSTOM_BASE_URL" => Some("https://custom.example/v1".to_string()),
                        name if name.ends_with("_API_KEY") => Some("test-key".to_string()),
                        _ => None,
                    },
                    || Ok(None),
                    None,
                )
                .unwrap();
            assert_eq!(resolved.inference_surface, inference, "{alias}");
            let resolved = backend
                .resolve_discovery_with(
                    |name| match name {
                        "CUSTOM_BASE_URL" => Some("https://custom.example/v1".to_string()),
                        name if name.ends_with("_API_KEY") => Some("test-key".to_string()),
                        _ => None,
                    },
                    || Ok(None),
                    None,
                )
                .unwrap();
            assert_eq!(resolved.discovery_surface, discovery, "{alias}");
        }
    }

    #[test]
    fn later_capability_construction_re_resolves_current_api_key() {
        let providers = HashMap::from([(
            "deepseek".to_string(),
            ProviderEntry {
                api_key_env: Some("DIRGE_BACKEND_TEST_KEY".into()),
                ..Default::default()
            },
        )]);
        let backend = ProviderBackend::new("deepseek", &providers, None).unwrap();
        let first = backend
            .resolve_inference_with(
                None,
                |name| (name == "DIRGE_BACKEND_TEST_KEY").then(|| "first-key".to_string()),
                || Ok(None),
                None,
            )
            .unwrap();
        let second = backend
            .resolve_discovery_with(
                |name| (name == "DIRGE_BACKEND_TEST_KEY").then(|| "second-key".to_string()),
                || Ok(None),
                None,
            )
            .unwrap();
        assert_eq!(first.credential.secret(), "first-key");
        assert_eq!(
            second
                .headers
                .get("authorization")
                .and_then(|value| value.to_str().ok()),
            Some("Bearer second-key")
        );
    }

    #[test]
    fn openai_oauth_surface_and_discovery_url_remain_codex_compatible() {
        let backend = ProviderBackend::new("openai", &HashMap::new(), None).unwrap();
        let oauth = || {
            Ok(Some(OpenAiOAuthCredential::new(
                "oauth-access",
                "oauth-refresh",
                None,
                Some("acct".into()),
                i64::MAX,
            )))
        };
        let inference = backend
            .resolve_inference_with(None, no_env, oauth, None)
            .unwrap();
        assert_eq!(
            inference.inference_surface,
            InferenceSurface::ResponsesCompatible
        );
        assert_eq!(
            inference.inference_base_url.as_deref(),
            Some(CHATGPT_CODEX_BASE_URL)
        );
        let discovery = backend.resolve_discovery_with(no_env, oauth, None).unwrap();
        assert_eq!(
            discovery.discovery_surface,
            DiscoverySurface::OpenAiModelsCompatible
        );
        assert_eq!(discovery.discovery_base_url, CHATGPT_CODEX_BASE_URL);
    }

    #[test]
    fn chatgpt_discovery_preserves_optional_account_id_behavior() {
        let providers = HashMap::from([(
            "openai".to_string(),
            ProviderEntry {
                auth: Some(ProviderAuth::ChatGpt),
                ..Default::default()
            },
        )]);
        let backend = ProviderBackend::new("openai", &providers, None).unwrap();
        let headers = ProviderAuthHeaders {
            bearer_token: "test-token".into(),
            chatgpt_account_id: None,
            chatgpt_bearer_is_dirge_oauth: false,
        };
        let discovery = backend
            .resolve_discovery_with(no_env, || Ok(None), Some(headers))
            .expect("catalog discovery historically allows an accountless bearer");
        assert_eq!(
            discovery.discovery_surface,
            DiscoverySurface::OpenAiModelsCompatible
        );
    }

    #[test]
    fn configured_url_and_auth_guard_are_shared_by_both_capabilities() {
        let providers = HashMap::from([(
            "anthropic".to_string(),
            ProviderEntry {
                base_url: Some("http://proxy.local/v1".to_string()),
                auth: Some(ProviderAuth::Anthropic),
                allow_insecure: true,
                ..Default::default()
            },
        )]);
        let backend = ProviderBackend::new("anthropic", &providers, None).unwrap();
        let headers = ProviderAuthHeaders {
            bearer_token: "test-token".into(),
            chatgpt_account_id: None,
            chatgpt_bearer_is_dirge_oauth: false,
        };
        let inference_error = match backend.resolve_inference_with(
            None,
            no_env,
            || Ok(None),
            Some(headers.clone()),
        ) {
            Ok(_) => panic!("insecure OAuth inference URL must fail"),
            Err(error) => error,
        };
        let discovery_error =
            match backend.resolve_discovery_with(no_env, || Ok(None), Some(headers)) {
                Ok(_) => panic!("insecure OAuth discovery URL must fail"),
                Err(error) => error,
            };
        assert!(inference_error.to_string().contains("https base URL"));
        assert!(discovery_error.to_string().contains("https base URL"));
    }
}
