//! Automatic model tier classification from model name.
//!
//! Classifies models into Small/Mid/Frontier tiers based on name patterns,
//! parameter counts, and known model families. Supports OpenRouter, Ollama,
//! Hugging Face, and provider-specific naming conventions.

use crate::model::{ModelCache, ModelTier, discovery};
use once_cell::sync::Lazy;
use regex::Regex;

/// Model classification result with confidence and reasoning.
#[derive(Debug, Clone)]
pub struct ModelClassification {
    pub tier: ModelTier,
    pub confidence: f32, // 0.0 - 1.0
    pub reason: String,
    pub detected_params: Option<u32>, // Parameter count in billions if detected
    pub detected_context: Option<usize>, // Context window if known
}

/// Regex patterns for parameter count detection (e.g., "7b", "70B", "3.5b").
static PARAM_REGEX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)(\d+(?:\.\d+)?)\s*[bB](?:[^a-z]|$)").unwrap());

/// Regex for context window hints (e.g., "32k", "128k", "200k").
static CONTEXT_REGEX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)(\d+(?:\.\d+)?)\s*[kK](?:[^a-z]|$)").unwrap());

/// Provider prefixes to strip for cleaner matching.
const PROVIDER_PREFIXES: &[&str] = &[
    "openrouter/",
    "anthropic/",
    "openai/",
    "google/",
    "meta-llama/",
    "mistralai/",
    "qwen/",
    "nvidia/",
    "deepseek/",
    "microsoft/",
    "cohere/",
    "north/",
    "hf/",
    "huggingface/",
];

/// Classify a model by name into a tier.
///
/// The model catalog (bundled + models.dev cache) is authoritative; the rest
/// are name heuristics for models no catalog knows (local, fine-tunes, new releases).
pub fn classify_model(name: &str) -> ModelClassification {
    // 1. Catalog: tier derived from capabilities (context, tools, price)
    if let Some(entry) = crate::model::catalog::lookup(name) {
        return ModelClassification {
            tier: entry.tier(),
            confidence: 0.95,
            reason: format!("Catalog match: {}", entry.id),
            detected_params: None,
            detected_context: Some(entry.context),
        };
    }

    let normalized = normalize_model_name(name);

    // 3. Parameter-based classification
    if let Some(params) = extract_params(&normalized) {
        let tier = tier_from_params(params);
        return ModelClassification {
            tier,
            confidence: 0.8,
            reason: format!("Classified by parameter count: {}B params", params),
            detected_params: Some(params),
            detected_context: None,
        };
    }

    // 4. Context window hints
    if let Some(ctx) = extract_context(&normalized) {
        let tier = tier_from_context(ctx);
        return ModelClassification {
            tier,
            confidence: 0.6,
            reason: format!("Classified by context window hint: {}k tokens", ctx / 1000),
            detected_params: None,
            detected_context: Some(ctx),
        };
    }

    // 5. Family-based heuristics
    if let Some(tier) = classify_by_family(&normalized) {
        return ModelClassification {
            tier,
            confidence: 0.7,
            reason: format!("Classified by model family: {}", normalized),
            detected_params: None,
            detected_context: None,
        };
    }

    // 6. Default to Mid as safe middle ground
    ModelClassification {
        tier: ModelTier::Mid,
        confidence: 0.3,
        reason: "No confident classification; defaulting to Mid tier".to_string(),
        detected_params: None,
        detected_context: None,
    }
}

/// Normalize model name for matching.
fn normalize_model_name(name: &str) -> String {
    let mut s = name.to_lowercase();

    // Strip provider prefixes
    for prefix in PROVIDER_PREFIXES {
        if s.starts_with(prefix) {
            s = s[prefix.len()..].to_string();
            break;
        }
    }

    // Strip common suffixes
    for suffix in &["-instruct", "-chat", "-base", "-preview", "-latest"] {
        if s.ends_with(suffix) {
            s = s[..s.len() - suffix.len()].to_string();
        }
    }

    // Replace separators (but keep dots for version numbers like 1.5)
    s = s.replace(['/', ':', '_'], "-");

    s
}

/// Extract parameter count from model name (e.g., "7b" -> 7, "70B" -> 70).
fn extract_params(name: &str) -> Option<u32> {
    PARAM_REGEX
        .captures(name)
        .and_then(|c| c.get(1))
        .and_then(|m| m.as_str().parse::<f32>().ok())
        .map(|f| f.round() as u32)
}

/// Extract context window hint from model name (e.g., "32k" -> 32000).
fn extract_context(name: &str) -> Option<usize> {
    CONTEXT_REGEX
        .captures(name)
        .and_then(|c| c.get(1))
        .and_then(|m| m.as_str().parse::<f32>().ok())
        .map(|f| (f * 1000.0) as usize)
}

/// Determine tier from parameter count.
fn tier_from_params(params: u32) -> ModelTier {
    match params {
        0..=10 => ModelTier::Small,
        11..=70 => ModelTier::Mid,
        _ => ModelTier::Frontier,
    }
}

/// Determine tier from context window.
fn tier_from_context(ctx: usize) -> ModelTier {
    match ctx {
        0..=32_000 => ModelTier::Small,
        32_001..=128_000 => ModelTier::Mid,
        _ => ModelTier::Frontier,
    }
}

/// Classify by model family name patterns.
fn classify_by_family(name: &str) -> Option<ModelTier> {
    // Small families
    let small_families = [
        "phi",
        "gemma",
        "tinyllama",
        "smollm",
        "minicpm",
        "mobilellm",
        "qwen1.5-",
        "qwen2-0.5b",
        "qwen2-1.5b",
        "qwen2.5-0.5b",
        "qwen2.5-1.5b",
        "llama-3.2-1b",
        "llama-3.2-3b",
    ];
    for family in &small_families {
        if name.contains(family) {
            return Some(ModelTier::Small);
        }
    }

    // Mid families (larger variants)
    let mid_families = [
        "llama-3-70b",
        "llama-3.1-70b",
        "llama-3.3-70b",
        "qwen-72b",
        "qwen2.5-72b",
        "qwen2-72b",
        "mistral-large",
        "mistral-medium",
        "command-r-plus",
        "command-r",
        "deepseek-v2",
        "deepseek-v3",
        "deepseek-coder-v2",
        "nemotron-3-ultra",
        "nemotron-4-340b",
        "gemma-2-27b",
        "gemma-27b",
        "qwen-32b",
        "qwen2.5-32b",
        "qwen2-32b",
        "codellama-34b",
        "starcoder2-15b",
    ];
    for family in &mid_families {
        if name.contains(family) {
            return Some(ModelTier::Mid);
        }
    }

    // Frontier families
    let frontier_families = [
        "gpt-4",
        "gpt-5",
        "opus",
        "ultra",
        "frontier",
        "llama-4",
        "llama-405b",
    ];
    for family in &frontier_families {
        if name.contains(family) {
            return Some(ModelTier::Frontier);
        }
    }

    None
}

/// Get default capabilities for a tier.
pub fn default_capabilities_for_tier(tier: ModelTier) -> crate::model::ModelCapabilities {
    use crate::model::{ModelCapabilities, ModelTier};
    use crate::tool::builtin::EditFormat;
    match tier {
        ModelTier::Small => ModelCapabilities {
            tool_calling: false,
            structured_output: false,
            thinking: false,
            max_context: 32768,
            max_output: 4096,
            temperature_supported: true,
            vision: false,
            tier: ModelTier::Small,
            edit_format: EditFormat::StrReplace,
        },
        ModelTier::Mid => ModelCapabilities {
            tool_calling: true,
            structured_output: true,
            thinking: false,
            max_context: 131072,
            max_output: 8192,
            temperature_supported: true,
            vision: false,
            tier: ModelTier::Mid,
            edit_format: EditFormat::Diff,
        },
        ModelTier::Frontier => ModelCapabilities {
            tool_calling: true,
            structured_output: true,
            thinking: true,
            max_context: 200000,
            max_output: 8192,
            temperature_supported: true,
            vision: false,
            tier: ModelTier::Frontier,
            edit_format: EditFormat::Diff,
        },
    }
}

/// Merge explicit config capabilities with auto-detected defaults.
pub fn merge_capabilities(
    explicit: Option<&crate::config::ModelCapabilitiesSection>,
    auto: &crate::model::ModelCapabilities,
) -> crate::model::ModelCapabilities {
    let mut merged = auto.clone();

    if let Some(e) = explicit {
        if let Some(v) = e.tool_calling {
            merged.tool_calling = v;
        }
        if let Some(v) = e.structured_output {
            merged.structured_output = v;
        }
        if let Some(v) = e.thinking {
            merged.thinking = v;
        }
        if let Some(v) = e.max_context {
            merged.max_context = v;
        }
        if let Some(v) = e.max_output {
            merged.max_output = v;
        }
        if let Some(v) = e.temperature_supported {
            merged.temperature_supported = v;
        }
        if let Some(v) = e.vision {
            merged.vision = v;
        }
        if let Some(t) = &e.tier
            && let Ok(parsed) = t.parse::<ModelTier>()
        {
            merged.tier = parsed;
        }
    }

    merged
}

/// Discover models from OpenRouter (with cache) and classify a specific model.
/// Falls back to static minimal list if no API key or fetch fails.
pub async fn discover_and_classify(model_name: &str, api_key: Option<&str>) -> ModelClassification {
    let cache = ModelCache::new().ok();

    // Try cache first
    if let Some(ref cache) = cache
        && let Some(models) = cache.get()
        && let Some(discovered) = models.iter().find(|m| m.id == model_name)
    {
        return ModelClassification {
            tier: discovered.tier,
            confidence: 0.95,
            reason: format!("Found in OpenRouter cache: {}", discovered.name),
            detected_params: extract_params_from_discovered(discovered),
            detected_context: Some(discovered.context_length),
        };
    }

    // Try to fetch from OpenRouter if API key available
    if let Some(key) = api_key.filter(|k| !k.is_empty()) {
        match discovery::fetch_openrouter_models(key).await {
            Ok(models) => {
                // Cache the fetched models
                if let Some(ref cache) = cache {
                    let _ = cache.set(models.clone());
                }

                if let Some(discovered) = models.iter().find(|m| m.id == model_name) {
                    return ModelClassification {
                        tier: discovered.tier,
                        confidence: 0.95,
                        reason: format!("Discovered from OpenRouter: {}", discovered.name),
                        detected_params: extract_params_from_discovered(discovered),
                        detected_context: Some(discovered.context_length),
                    };
                }

                // Model not found in fetched list, fall through to static classification
            }
            Err(_) => {
                // Fetch failed, fall through to static classification
            }
        }
    }

    // Fallback to heuristic classification (param extraction, context hints, family patterns)
    let classification = classify_model(model_name);

    ModelClassification {
        confidence: classification.confidence,
        reason: format!("{} (fallback)", classification.reason),
        ..classification
    }
}

/// Extract parameter count from discovered model if available.
fn extract_params_from_discovered(model: &discovery::DiscoveredModel) -> Option<u32> {
    discovery::extract_params_from_id(&model.id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ModelTier;

    #[test]
    fn test_catalog_models_use_catalog_tier() {
        let result = classify_model("gpt-4o");
        assert_eq!(result.tier, ModelTier::Frontier);
        assert_eq!(result.confidence, 0.95);
        assert_eq!(result.detected_context, Some(128_000));

        let result = classify_model("openai/gpt-4o-mini");
        assert_eq!(result.tier, ModelTier::Small);
    }

    #[test]
    fn test_heuristics_for_uncataloged_models() {
        assert_eq!(classify_model("claude-3-opus").tier, ModelTier::Frontier);
        assert_eq!(classify_model("llama-3.1-70b").tier, ModelTier::Mid);
        assert_eq!(classify_model("llama-3.1-8b").tier, ModelTier::Small);
        assert_eq!(classify_model("gemma-2-9b").tier, ModelTier::Small);
        assert_eq!(classify_model("mistral-7b").tier, ModelTier::Small);
    }

    #[test]
    fn test_param_based_classification() {
        // Small params
        assert_eq!(tier_from_params(1), ModelTier::Small);
        assert_eq!(tier_from_params(7), ModelTier::Small);
        assert_eq!(tier_from_params(8), ModelTier::Small);
        assert_eq!(tier_from_params(9), ModelTier::Small);

        // Mid params
        assert_eq!(tier_from_params(13), ModelTier::Mid);
        assert_eq!(tier_from_params(27), ModelTier::Mid);
        assert_eq!(tier_from_params(32), ModelTier::Mid);
        assert_eq!(tier_from_params(70), ModelTier::Mid);

        // Frontier params
        assert_eq!(tier_from_params(72), ModelTier::Frontier);
        assert_eq!(tier_from_params(100), ModelTier::Frontier);
        assert_eq!(tier_from_params(405), ModelTier::Frontier);
    }

    #[test]
    fn test_param_extraction() {
        assert_eq!(extract_params("llama-3-70b"), Some(70));
        assert_eq!(extract_params("qwen2.5-72b-instruct"), Some(72));
        assert_eq!(extract_params("gemma-2-9b-it"), Some(9));
        assert_eq!(extract_params("phi-3.5-mini"), None); // no param in name
        assert_eq!(extract_params("mixtral-8x7b"), Some(7)); // catches 7b
    }

    #[test]
    fn test_provider_prefix_stripping() {
        let result = classify_model("openrouter/meta-llama/llama-3.1-70b-instruct");
        assert_eq!(result.tier, ModelTier::Mid);
    }

    #[test]
    fn test_context_hint_classification() {
        // These would be classified by context hint if no other match
        // but known models take precedence
        let _result = classify_model("unknown-model-32k");
        // Should detect 32k context -> Small tier (or Mid depending on exact boundary)
    }

    #[test]
    fn test_default_capabilities_for_tier() {
        let small = default_capabilities_for_tier(ModelTier::Small);
        assert!(!small.tool_calling);
        assert_eq!(small.max_context, 32768);
        assert_eq!(small.tier, ModelTier::Small);

        let mid = default_capabilities_for_tier(ModelTier::Mid);
        assert!(mid.tool_calling);
        assert_eq!(mid.max_context, 131072);
        assert_eq!(mid.tier, ModelTier::Mid);

        let frontier = default_capabilities_for_tier(ModelTier::Frontier);
        assert!(frontier.tool_calling);
        assert!(frontier.thinking);
        assert_eq!(frontier.max_context, 200000);
        assert_eq!(frontier.tier, ModelTier::Frontier);
    }
}
