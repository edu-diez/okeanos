//! Provider registry: the single place that turns configuration into a model.
//!
//! Replaces per-call-site provider construction. Adding a provider means
//! adding a [`ProviderKind`] arm here, nowhere else.

use crate::config::{Config, ProviderSection};
use crate::context::tokens::ModelPricing;
use crate::model::{
    AnthropicModel, GoogleModel, Model, ModelCapabilities, OpenAIModel, OpenAIResponsesModel,
    OpenRouterModel, catalog,
};
use std::sync::Arc;

/// Wire protocol family of a provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    Anthropic,
    OpenAI,
    Google,
    OpenAICompatible,
}

impl ProviderKind {
    /// Unknown strings are rejected by config validation; here they degrade
    /// to the generic OpenAI-compatible protocol.
    pub fn parse(s: Option<&str>) -> Self {
        match s {
            Some("anthropic") => Self::Anthropic,
            Some("openai") => Self::OpenAI,
            Some("google") => Self::Google,
            _ => Self::OpenAICompatible,
        }
    }

    pub fn default_base_url(self) -> &'static str {
        match self {
            Self::Anthropic => crate::model::ANTHROPIC_DEFAULT_BASE_URL,
            Self::OpenAI => crate::model::OPENAI_DEFAULT_BASE_URL,
            Self::Google => crate::model::GOOGLE_DEFAULT_BASE_URL,
            Self::OpenAICompatible => crate::model::DEFAULT_BASE_URL,
        }
    }
}

/// OpenAI wire API selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WireApi {
    #[default]
    Chat,
    Responses,
}

impl WireApi {
    pub fn parse(s: Option<&str>) -> Self {
        match s {
            Some("responses") => Self::Responses,
            _ => Self::Chat,
        }
    }
}

/// Everything needed to reach a provider.
#[derive(Debug, Clone)]
pub struct ProviderSpec {
    pub kind: ProviderKind,
    pub wire_api: WireApi,
    pub base_url: String,
    pub api_key: String,
}

impl ProviderSpec {
    /// `None` when the section has no API key (the caller falls back to the mock model).
    pub fn from_section(section: &ProviderSection) -> Option<Self> {
        let api_key = section.api_key.as_deref().filter(|k| !k.is_empty())?;
        let kind = ProviderKind::parse(section.kind.as_deref());
        Some(Self {
            kind,
            wire_api: WireApi::parse(section.wire_api.as_deref()),
            base_url: section
                .base_url
                .clone()
                .unwrap_or_else(|| kind.default_base_url().to_string()),
            api_key: api_key.to_string(),
        })
    }
}

/// Resolve which provider serves `name`, returning the provider-local model id.
///
/// `"<provider>/<model>"` selects `[providers.<provider>]` when that table
/// exists; anything else (including OpenRouter ids such as
/// `anthropic/claude-sonnet-4`) uses the default `[provider]` and is passed
/// through untouched.
pub fn resolve(config: &Config, name: &str) -> (Option<ProviderSpec>, String) {
    if let Some((prefix, rest)) = name.split_once('/')
        && let Some(section) = config.providers.get(prefix)
    {
        return (ProviderSpec::from_section(section), rest.to_string());
    }
    (
        config
            .provider
            .as_ref()
            .and_then(ProviderSpec::from_section),
        name.to_string(),
    )
}

/// Construct the model for a provider.
pub fn build_model(
    spec: &ProviderSpec,
    model_id: &str,
    caps: ModelCapabilities,
    pricing: Option<ModelPricing>,
) -> Arc<dyn Model + Send + Sync> {
    let client = reqwest::Client::new();
    let (url, key) = (spec.base_url.as_str(), spec.api_key.as_str());
    match (spec.kind, spec.wire_api) {
        (ProviderKind::Anthropic, _) => Arc::new(AnthropicModel::new(
            client, url, key, model_id, caps, pricing,
        )),
        (ProviderKind::Google, _) => {
            Arc::new(GoogleModel::new(client, url, key, model_id, caps, pricing))
        }
        (ProviderKind::OpenAI, WireApi::Responses) => Arc::new(OpenAIResponsesModel::new(
            client, url, key, model_id, caps, pricing,
        )),
        (ProviderKind::OpenAI, WireApi::Chat) => {
            Arc::new(OpenAIModel::new(client, url, key, model_id, caps, pricing))
        }
        (ProviderKind::OpenAICompatible, _) => Arc::new(OpenRouterModel::new(
            client, url, key, model_id, caps, pricing,
        )),
    }
}

/// Build the model named `name` from configuration, with catalog pricing.
/// `None` when no provider with an API key applies.
pub fn build_from_config(
    config: &Config,
    name: &str,
    caps: &ModelCapabilities,
) -> Option<Arc<dyn Model + Send + Sync>> {
    let (spec, model_id) = resolve(config, name);
    let spec = spec?;
    let pricing = catalog::pricing(&model_id);
    Some(build_model(&spec, &model_id, caps.clone(), pricing))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(toml_src: &str) -> Config {
        toml::from_str(toml_src).unwrap()
    }

    #[test]
    fn resolves_named_provider_prefix_only_when_declared() {
        let cfg = config(
            r#"
[provider]
kind = "openai-compatible"
api_key = "or-key"

[providers.anthropic]
kind = "anthropic"
api_key = "ant-key"
"#,
        );
        let (spec, id) = resolve(&cfg, "anthropic/claude-sonnet-4");
        assert_eq!(spec.unwrap().kind, ProviderKind::Anthropic);
        assert_eq!(id, "claude-sonnet-4");

        // Not a declared provider: stays an OpenRouter-style id on the default provider.
        let (spec, id) = resolve(&cfg, "meta-llama/llama-3.1-8b");
        assert_eq!(spec.unwrap().kind, ProviderKind::OpenAICompatible);
        assert_eq!(id, "meta-llama/llama-3.1-8b");
    }

    #[test]
    fn section_without_key_yields_no_spec_and_wire_api_parses() {
        let cfg = config(
            r#"
[provider]
kind = "openai"
wire_api = "responses"
api_key = "k"
base_url = "https://proxy.example"
"#,
        );
        let spec = ProviderSpec::from_section(cfg.provider.as_ref().unwrap()).unwrap();
        assert_eq!(spec.wire_api, WireApi::Responses);
        assert_eq!(spec.base_url, "https://proxy.example");
        assert!(ProviderSpec::from_section(&ProviderSection::default()).is_none());
        let defaulted = ProviderSpec::from_section(&ProviderSection {
            kind: Some("google".into()),
            api_key: Some("k".into()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(defaulted.base_url, crate::model::GOOGLE_DEFAULT_BASE_URL);
    }

    #[test]
    fn builds_each_provider_with_its_model_id() {
        for (kind, wire) in [
            ("anthropic", None),
            ("openai", None),
            ("openai", Some("responses")),
            ("google", None),
            ("openai-compatible", None),
        ] {
            let spec = ProviderSpec::from_section(&ProviderSection {
                kind: Some(kind.into()),
                wire_api: wire.map(Into::into),
                api_key: Some("k".into()),
                ..Default::default()
            })
            .unwrap();
            let caps = crate::model::default_capabilities_for_tier(crate::model::ModelTier::Mid);
            let model = build_model(&spec, "some-model", caps, None);
            assert_eq!(model.model_id(), "some-model");
        }
    }
}
