//! Runtime provider/model route preparation.
//!
//! A route is prepared without mutating the live UI state. Callers build the
//! replacement agent from [`PreparedRoute::client`] and only then install the
//! client, agent, and session fields together. This keeps a failed provider
//! construction from leaving the session half-switched.

use std::collections::HashMap;

use crate::config::{ProviderAuth, ProviderEntry};

use super::{AnyClient, ModelSwitch, create_client_with_auth, resolve_model_name};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveRoute {
    pub provider_alias: String,
    pub model_id: String,
    pub model_explicit: bool,
}

pub struct PreparedRoute {
    pub route: ActiveRoute,
    pub client: AnyClient,
}

/// Resolve a user-entered model id to the provider alias it should use.
pub fn provider_for_model(
    providers: &HashMap<String, ProviderEntry>,
    active_provider: &str,
    model_id: &str,
) -> anyhow::Result<String> {
    match super::resolve_model_switch(providers, active_provider, model_id) {
        ModelSwitch::Keep => canonical_provider_alias(providers, active_provider),
        ModelSwitch::Switch(alias) => canonical_provider_alias(providers, &alias),
        ModelSwitch::NoProviderForFamily(family) => anyhow::bail!(
            "'{model_id}' matches the {family} model family, but no {family} provider is configured. Add a provider of type {family} to config.json to switch to it."
        ),
    }
}

/// Build and authenticate a target client without touching live state.
pub fn prepare_active_route(
    provider_alias: &str,
    requested_model: &str,
    model_explicit: bool,
    providers: &HashMap<String, ProviderEntry>,
    default_auth: Option<ProviderAuth>,
) -> anyhow::Result<PreparedRoute> {
    let provider_alias = canonical_provider_alias(providers, provider_alias)?;
    let client = create_client_with_auth(&provider_alias, None, providers, default_auth)?;
    let model_id = resolve_model_name(&client, requested_model, model_explicit);
    Ok(PreparedRoute {
        route: ActiveRoute {
            provider_alias,
            model_id,
            model_explicit,
        },
        client,
    })
}

fn canonical_provider_alias(
    providers: &HashMap<String, ProviderEntry>,
    requested: &str,
) -> anyhow::Result<String> {
    if let Some(alias) = providers
        .keys()
        .find(|alias| alias.eq_ignore_ascii_case(requested))
    {
        return Ok(alias.clone());
    }
    // Built-ins do not need an explicit providers-map entry, but still pass
    // through provider resolution so an unknown alias fails before mutation.
    if super::resolve_provider_info(requested, providers).is_some() {
        return Ok(requested.to_ascii_lowercase());
    }
    anyhow::bail!("unknown provider alias '{requested}'")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalizes_configured_alias_case() {
        let providers = HashMap::from([(
            "My-OpenAI".to_string(),
            ProviderEntry {
                provider_type: Some("openai".to_string()),
                ..Default::default()
            },
        )]);
        assert_eq!(
            canonical_provider_alias(&providers, "my-openai").unwrap(),
            "My-OpenAI"
        );
    }

    #[test]
    fn rejects_unknown_alias() {
        assert!(canonical_provider_alias(&HashMap::new(), "made-up").is_err());
    }
}
