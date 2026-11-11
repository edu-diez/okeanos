//! Model discovery from OpenRouter API.
//!
//! Fetches model metadata from OpenRouter and maps to internal capabilities.

use crate::model::ModelTier;
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Errors during model discovery.
#[derive(Debug, Error)]
pub enum DiscoveryError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("API returned error: {0}")]
    ApiError(String),
    #[error("JSON parsing failed: {0}")]
    JsonParse(#[from] serde_json::Error),
    #[error("Cache error: {0}")]
    Cache(#[from] std::io::Error),
    #[error("No API key provided")]
    NoApiKey,
}

/// Pricing information from OpenRouter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelPricing {
    pub prompt: String,
    pub completion: String,
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default)]
    pub request: Option<String>,
}

/// Architecture information from OpenRouter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelArchitecture {
    pub modality: String,
    pub tokenizer: String,
    #[serde(default)]
    pub instruct_type: Option<String>,
}

/// OpenRouter model response item.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenRouterModel {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub context_length: usize,
    pub max_completion_tokens: Option<usize>,
    pub pricing: ModelPricing,
    pub architecture: ModelArchitecture,
    #[serde(default)]
    pub supported_parameters: Vec<String>,
}

/// Top-level OpenRouter API response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenRouterModelsResponse {
    pub data: Vec<OpenRouterModel>,
}

/// Discovered model with mapped capabilities.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredModel {
    pub id: String,
    pub name: String,
    pub context_length: usize,
    pub max_output_tokens: usize,
    pub pricing: ModelPricing,
    pub supports_tools: bool,
    pub supports_structured_output: bool,
    pub description: Option<String>,
    pub tier: ModelTier,
}

impl DiscoveredModel {
    /// Create a DiscoveredModel from OpenRouter model data.
    pub fn from_openrouter(model: OpenRouterModel) -> Self {
        let supports_tools = model
            .supported_parameters
            .iter()
            .any(|p| p.to_lowercase().contains("tool") || p.to_lowercase().contains("function"));

        let supports_structured_output = model
            .supported_parameters
            .iter()
            .any(|p| p.to_lowercase().contains("json") || p.to_lowercase().contains("schema"));

        // Use max_completion_tokens if available, otherwise estimate from context
        let max_output_tokens = model
            .max_completion_tokens
            .unwrap_or((model.context_length / 4).min(8192));

        let tier = map_to_tier(&model);

        Self {
            id: model.id,
            name: model.name,
            context_length: model.context_length,
            max_output_tokens,
            pricing: model.pricing,
            supports_tools,
            supports_structured_output,
            description: model.description,
            tier,
        }
    }
}

/// Map OpenRouter model to tier using parameter count, context length, and capabilities.
fn map_to_tier(model: &OpenRouterModel) -> ModelTier {
    // 1. Try to extract parameter count from model ID
    if let Some(params) = extract_params_from_id(&model.id) {
        if params >= 72 {
            return ModelTier::Frontier;
        }
        if params >= 11 {
            return ModelTier::Mid;
        }
        return ModelTier::Small;
    }

    // 2. Use context length as proxy
    if model.context_length >= 128_000 {
        return ModelTier::Frontier;
    }
    if model.context_length >= 32_000 {
        return ModelTier::Mid;
    }

    // 3. Use capabilities as tiebreaker
    let has_tools = model
        .supported_parameters
        .iter()
        .any(|p| p.to_lowercase().contains("tool") || p.to_lowercase().contains("function"));
    let has_structured = model
        .supported_parameters
        .iter()
        .any(|p| p.to_lowercase().contains("json") || p.to_lowercase().contains("schema"));

    if has_tools && has_structured && model.context_length >= 32_000 {
        return ModelTier::Mid;
    }

    // Default to Mid for unknown models with tool support
    if has_tools {
        return ModelTier::Mid;
    }

    ModelTier::Small
}

/// Extract parameter count from model ID (e.g., "llama-3.1-70b" -> 70).
static PARAM_REGEX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)(\d+(?:\.\d+)?)\s*[bB](?:[^a-z]|$)").unwrap());

/// Extract parameter count from model ID. Public for use by classifier.
pub fn extract_params_from_id(id: &str) -> Option<u32> {
    PARAM_REGEX
        .captures(id)
        .and_then(|c| c.get(1))
        .and_then(|m| m.as_str().parse::<f32>().ok())
        .map(|f| f.round() as u32)
}

/// Fetch models from OpenRouter API.
pub async fn fetch_openrouter_models(
    api_key: &str,
) -> Result<Vec<DiscoveredModel>, DiscoveryError> {
    if api_key.is_empty() {
        return Err(DiscoveryError::NoApiKey);
    }

    let client = reqwest::Client::new();
    let response = client
        .get("https://openrouter.ai/api/v1/models")
        .header("Authorization", format!("Bearer {}", api_key))
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status();
        let error_text = response.text().await.unwrap_or_default();
        return Err(DiscoveryError::ApiError(format!(
            "HTTP {}: {}",
            status, error_text
        )));
    }

    let data: OpenRouterModelsResponse = response.json().await?;
    let models: Vec<DiscoveredModel> = data
        .data
        .into_iter()
        .map(DiscoveredModel::from_openrouter)
        .collect();

    Ok(models)
}

/// Minimal fallback models when no API key or fetch fails.
/// Returns empty - we rely on heuristic classification instead of hardcoded anchors.
pub fn minimal_fallback_models() -> Vec<DiscoveredModel> {
    vec![]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_params_from_id() {
        assert_eq!(extract_params_from_id("llama-3.1-70b"), Some(70));
        assert_eq!(extract_params_from_id("qwen2.5-72b-instruct"), Some(72));
        assert_eq!(extract_params_from_id("gemma-2-9b-it"), Some(9));
        assert_eq!(extract_params_from_id("phi-3.5-mini"), None);
        assert_eq!(extract_params_from_id("gpt-4o"), None);
        assert_eq!(extract_params_from_id("mixtral-8x7b"), Some(7));
    }

    #[test]
    fn test_map_to_tier_by_params() {
        let model = OpenRouterModel {
            id: "llama-3.1-70b".to_string(),
            name: "Llama 3.1 70B".to_string(),
            description: None,
            context_length: 128_000,
            max_completion_tokens: Some(8192),
            pricing: ModelPricing {
                prompt: "0.8".to_string(),
                completion: "0.8".to_string(),
                image: None,
                request: None,
            },
            architecture: ModelArchitecture {
                modality: "text".to_string(),
                tokenizer: "llama".to_string(),
                instruct_type: None,
            },
            supported_parameters: vec!["tools".to_string()],
        };
        assert_eq!(map_to_tier(&model), ModelTier::Mid);

        let model = OpenRouterModel {
            id: "llama-3.1-405b".to_string(),
            name: "Llama 3.1 405B".to_string(),
            description: None,
            context_length: 128_000,
            max_completion_tokens: Some(8192),
            pricing: ModelPricing {
                prompt: "3.0".to_string(),
                completion: "3.0".to_string(),
                image: None,
                request: None,
            },
            architecture: ModelArchitecture {
                modality: "text".to_string(),
                tokenizer: "llama".to_string(),
                instruct_type: None,
            },
            supported_parameters: vec!["tools".to_string()],
        };
        assert_eq!(map_to_tier(&model), ModelTier::Frontier);

        let model = OpenRouterModel {
            id: "gemma-2-9b".to_string(),
            name: "Gemma 2 9B".to_string(),
            description: None,
            context_length: 8192,
            max_completion_tokens: Some(2048),
            pricing: ModelPricing {
                prompt: "0.1".to_string(),
                completion: "0.1".to_string(),
                image: None,
                request: None,
            },
            architecture: ModelArchitecture {
                modality: "text".to_string(),
                tokenizer: "gemma".to_string(),
                instruct_type: None,
            },
            supported_parameters: vec![],
        };
        assert_eq!(map_to_tier(&model), ModelTier::Small);
    }

    #[test]
    fn test_map_to_tier_by_context() {
        // Large context but no params in ID -> Frontier
        let model = OpenRouterModel {
            id: "unknown-large-model".to_string(),
            name: "Unknown Large".to_string(),
            description: None,
            context_length: 200_000,
            max_completion_tokens: Some(8192),
            pricing: ModelPricing {
                prompt: "1.0".to_string(),
                completion: "1.0".to_string(),
                image: None,
                request: None,
            },
            architecture: ModelArchitecture {
                modality: "text".to_string(),
                tokenizer: "unknown".to_string(),
                instruct_type: None,
            },
            supported_parameters: vec![],
        };
        assert_eq!(map_to_tier(&model), ModelTier::Frontier);

        // Mid context
        let model = OpenRouterModel {
            id: "unknown-mid-model".to_string(),
            name: "Unknown Mid".to_string(),
            description: None,
            context_length: 64_000,
            max_completion_tokens: Some(4096),
            pricing: ModelPricing {
                prompt: "1.0".to_string(),
                completion: "1.0".to_string(),
                image: None,
                request: None,
            },
            architecture: ModelArchitecture {
                modality: "text".to_string(),
                tokenizer: "unknown".to_string(),
                instruct_type: None,
            },
            supported_parameters: vec![],
        };
        assert_eq!(map_to_tier(&model), ModelTier::Mid);

        // Small context
        let model = OpenRouterModel {
            id: "unknown-small-model".to_string(),
            name: "Unknown Small".to_string(),
            description: None,
            context_length: 4096,
            max_completion_tokens: Some(2048),
            pricing: ModelPricing {
                prompt: "1.0".to_string(),
                completion: "1.0".to_string(),
                image: None,
                request: None,
            },
            architecture: ModelArchitecture {
                modality: "text".to_string(),
                tokenizer: "unknown".to_string(),
                instruct_type: None,
            },
            supported_parameters: vec![],
        };
        assert_eq!(map_to_tier(&model), ModelTier::Small);
    }

    #[test]
    fn test_minimal_fallback_models() {
        let models = minimal_fallback_models();
        // Now returns empty - we rely on heuristic classification
        assert_eq!(models.len(), 0);
    }
}
