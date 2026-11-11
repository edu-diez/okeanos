//! Automatic model tier classification from model name.
//!
//! Classifies models into Small/Mid/Frontier tiers based on name patterns,
//! parameter counts, and known model families. Supports OpenRouter, Ollama,
//! Hugging Face, and provider-specific naming conventions.

use crate::model::{ModelCache, ModelTier, discovery};
use once_cell::sync::Lazy;
use regex::Regex;
use std::collections::HashMap;

/// Model classification result with confidence and reasoning.
#[derive(Debug, Clone)]
pub struct ModelClassification {
    pub tier: ModelTier,
    pub confidence: f32, // 0.0 - 1.0
    pub reason: String,
    pub detected_params: Option<u32>, // Parameter count in billions if detected
    pub detected_context: Option<usize>, // Context window if known
}

/// Known model database with explicit tier assignments.
static KNOWN_MODELS: Lazy<HashMap<&'static str, ModelTier>> = Lazy::new(|| {
    let mut m = HashMap::new();
    // Frontier tier
    m.insert("gpt-4o", ModelTier::Frontier);
    m.insert("gpt-4o-2024-05-13", ModelTier::Frontier);
    m.insert("gpt-4o-2024-08-06", ModelTier::Frontier);
    m.insert("gpt-4o-2024-11-20", ModelTier::Frontier);
    m.insert("gpt-4-turbo", ModelTier::Frontier);
    m.insert("gpt-4-turbo-2024-04-09", ModelTier::Frontier);
    m.insert("gpt-4", ModelTier::Frontier);
    m.insert("gpt-4-32k", ModelTier::Frontier);
    m.insert("gpt-4-0125-preview", ModelTier::Frontier);
    m.insert("gpt-4-1106-preview", ModelTier::Frontier);
    m.insert("claude-3-5-sonnet", ModelTier::Mid); // Sonnet is mid-tier
    m.insert("claude-3-5-sonnet-20241022", ModelTier::Mid);
    m.insert("claude-3-5-sonnet-20240620", ModelTier::Mid);
    m.insert("claude-3-opus", ModelTier::Frontier);
    m.insert("claude-3-opus-20240229", ModelTier::Frontier);
    m.insert("claude-3-haiku", ModelTier::Small);
    m.insert("claude-3-haiku-20240307", ModelTier::Small);
    m.insert("gemini-1.5-pro", ModelTier::Frontier);
    m.insert("gemini-1.5-flash", ModelTier::Mid);
    m.insert("gemini-1.0-pro", ModelTier::Mid);
    m.insert("gemini-1.0-ultra", ModelTier::Frontier);

    // Mid tier
    m.insert("gpt-3.5-turbo", ModelTier::Mid);
    m.insert("gpt-3.5-turbo-0125", ModelTier::Mid);
    m.insert("gpt-3.5-turbo-1106", ModelTier::Mid);
    m.insert("gpt-3.5-turbo-16k", ModelTier::Mid);

    // OpenRouter / community models - Mid tier
    m.insert("mistral-large", ModelTier::Mid);
    m.insert("mistral-large-2407", ModelTier::Mid);
    m.insert("mistral-large-2411", ModelTier::Mid);
    m.insert("mistral-medium", ModelTier::Mid);
    m.insert("command-r-plus", ModelTier::Mid);
    m.insert("command-r", ModelTier::Mid);
    m.insert("command-r-08-2024", ModelTier::Mid);
    m.insert("command-r-plus-08-2024", ModelTier::Mid);
    m.insert("qwen-2.5-72b", ModelTier::Mid);
    m.insert("qwen2.5-72b-instruct", ModelTier::Mid);
    m.insert("qwen2-72b-instruct", ModelTier::Mid);
    m.insert("llama-3.1-70b", ModelTier::Mid);
    m.insert("llama-3.1-70b-instruct", ModelTier::Mid);
    m.insert("llama-3-70b", ModelTier::Mid);
    m.insert("llama-3-70b-instruct", ModelTier::Mid);
    m.insert("nemotron-3-ultra", ModelTier::Mid);
    m.insert("nemotron-4-340b", ModelTier::Mid);
    m.insert("deepseek-v2.5", ModelTier::Mid);
    m.insert("deepseek-coder-v2", ModelTier::Mid);

    // Small tier - local/small models
    m.insert("gpt-4o-mini", ModelTier::Small);
    m.insert("gpt-4o-mini-2024-07-18", ModelTier::Small);
    m.insert("gpt-3.5-turbo-instruct", ModelTier::Small);
    m.insert("claude-3-haiku", ModelTier::Small);
    m.insert("gemini-1.5-flash-8b", ModelTier::Small);
    m.insert("gemini-flash-1.5-8b", ModelTier::Small);
    m.insert("mistral-7b", ModelTier::Small);
    m.insert("mistral-7b-instruct", ModelTier::Small);
    m.insert("mistral-nemo", ModelTier::Small);
    m.insert("mistral-nemo-2407", ModelTier::Small);
    m.insert("llama-3.1-8b", ModelTier::Small);
    m.insert("llama-3.1-8b-instruct", ModelTier::Small);
    m.insert("llama-3-8b", ModelTier::Small);
    m.insert("llama-3-8b-instruct", ModelTier::Small);
    m.insert("llama-3.2-1b", ModelTier::Small);
    m.insert("llama-3.2-1b-instruct", ModelTier::Small);
    m.insert("llama-3.2-3b", ModelTier::Small);
    m.insert("llama-3.2-3b-instruct", ModelTier::Small);
    m.insert("gemma-2b", ModelTier::Small);
    m.insert("gemma-2b-it", ModelTier::Small);
    m.insert("gemma-7b", ModelTier::Small);
    m.insert("gemma-7b-it", ModelTier::Small);
    m.insert("gemma-2-2b", ModelTier::Small);
    m.insert("gemma-2-2b-it", ModelTier::Small);
    m.insert("gemma-2-9b", ModelTier::Small);
    m.insert("gemma-2-9b-it", ModelTier::Small);
    m.insert("gemma-2-27b", ModelTier::Mid); // 27B is mid-tier
    m.insert("gemma-2-27b-it", ModelTier::Mid);
    m.insert("phi-3-mini", ModelTier::Small);
    m.insert("phi-3.5-mini", ModelTier::Small);
    m.insert("phi-4", ModelTier::Small);
    m.insert("qwen-2.5-7b", ModelTier::Small);
    m.insert("qwen-2.5-14b", ModelTier::Small);
    m.insert("qwen-2.5-32b", ModelTier::Mid); // 32B is mid-tier
    m.insert("qwen2.5-7b-instruct", ModelTier::Small);
    m.insert("qwen2.5-14b-instruct", ModelTier::Small);
    m.insert("qwen2.5-32b-instruct", ModelTier::Mid);
    m.insert("qwen2-7b-instruct", ModelTier::Small);
    m.insert("qwen2-14b-instruct", ModelTier::Small);
    m.insert("qwen2-32b-instruct", ModelTier::Mid);
    m.insert("deepseek-coder-6.7b", ModelTier::Small);
    m.insert("deepseek-coder-33b", ModelTier::Mid);
    m.insert("starcoder2-15b", ModelTier::Small);
    m.insert("codellama-7b", ModelTier::Small);
    m.insert("codellama-13b", ModelTier::Small);
    m.insert("codellama-34b", ModelTier::Mid);
    m.insert("north/mini-code", ModelTier::Small);
    m.insert("north/mini-code-7b", ModelTier::Small);
    // Normalized versions (without provider prefix)
    m.insert("mini-code", ModelTier::Small);
    m.insert("mini-code-7b", ModelTier::Small);

    m
});

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
pub fn classify_model(name: &str) -> ModelClassification {
    let normalized = normalize_model_name(name);

    // 1. Exact match in known models database
    if let Some(&tier) = KNOWN_MODELS.get(normalized.as_str()) {
        return ModelClassification {
            tier,
            confidence: 1.0,
            reason: format!("Exact match in known models database: {}", normalized),
            detected_params: None,
            detected_context: None,
        };
    }

    // 2. Fuzzy match: check if normalized name contains a known model as a distinct component
    // Sort by length descending to match longer names first
    let mut known_entries: Vec<_> = KNOWN_MODELS.iter().collect();
    known_entries.sort_by_key(|(k, _)| std::cmp::Reverse(k.len()));

    for (known_name, &tier) in known_entries {
        if is_distinct_match(&normalized, known_name) {
            return ModelClassification {
                tier,
                confidence: 0.85,
                reason: format!("Fuzzy match with known model: {}", known_name),
                detected_params: None,
                detected_context: None,
            };
        }
    }

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

/// Check if a known model name matches as a distinct component in the normalized name.
/// This avoids false matches like "gpt-4" matching inside "gpt-4o-mini".
fn is_distinct_match(normalized: &str, known: &str) -> bool {
    if !normalized.contains(known) {
        return false;
    }

    // Check if the match is at word boundaries (separated by -, _, or string boundaries)
    let mut search_start = 0;
    while let Some(pos) = normalized[search_start..].find(known) {
        let actual_pos = search_start + pos;
        let before_ok =
            actual_pos == 0 || matches!(normalized.as_bytes()[actual_pos - 1], b'-' | b'_' | b'.');
        let after_pos = actual_pos + known.len();
        let after_ok = after_pos >= normalized.len()
            || matches!(normalized.as_bytes()[after_pos], b'-' | b'_' | b'.');

        if before_ok && after_ok {
            return true;
        }
        search_start = actual_pos + 1;
    }

    false
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
    match tier {
        ModelTier::Small => ModelCapabilities {
            tool_calling: false,
            structured_output: false,
            thinking: false,
            max_context: 32768,
            max_output: 4096,
            temperature_supported: true,
            tier: ModelTier::Small,
        },
        ModelTier::Mid => ModelCapabilities {
            tool_calling: true,
            structured_output: true,
            thinking: false,
            max_context: 131072,
            max_output: 8192,
            temperature_supported: true,
            tier: ModelTier::Mid,
        },
        ModelTier::Frontier => ModelCapabilities {
            tool_calling: true,
            structured_output: true,
            thinking: true,
            max_context: 200000,
            max_output: 8192,
            temperature_supported: true,
            tier: ModelTier::Frontier,
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
    fn test_known_frontier_models() {
        let result = classify_model("gpt-4o");
        assert_eq!(result.tier, ModelTier::Frontier);
        assert_eq!(result.confidence, 1.0);

        let result = classify_model("claude-3-opus");
        assert_eq!(result.tier, ModelTier::Frontier);

        let result = classify_model("gemini-1.5-pro");
        assert_eq!(result.tier, ModelTier::Frontier);
    }

    #[test]
    fn test_known_mid_models() {
        let result = classify_model("claude-3-5-sonnet");
        assert_eq!(result.tier, ModelTier::Mid);

        let result = classify_model("gpt-3.5-turbo");
        assert_eq!(result.tier, ModelTier::Mid);

        let result = classify_model("mistral-large");
        assert_eq!(result.tier, ModelTier::Mid);

        let result = classify_model("llama-3.1-70b");
        assert_eq!(result.tier, ModelTier::Mid);

        let result = classify_model("qwen-2.5-72b");
        assert_eq!(result.tier, ModelTier::Mid);
    }

    #[test]
    fn test_known_small_models() {
        let result = classify_model("gpt-4o-mini");
        assert_eq!(result.tier, ModelTier::Small);

        let result = classify_model("claude-3-haiku");
        assert_eq!(result.tier, ModelTier::Small);

        let result = classify_model("llama-3.1-8b");
        assert_eq!(result.tier, ModelTier::Small);

        let result = classify_model("gemma-2-9b");
        assert_eq!(result.tier, ModelTier::Small);

        let result = classify_model("phi-4");
        assert_eq!(result.tier, ModelTier::Small);

        let result = classify_model("mistral-7b");
        assert_eq!(result.tier, ModelTier::Small);

        let result = classify_model("north/mini-code");
        assert_eq!(result.tier, ModelTier::Small);
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

        let result = classify_model("anthropic/claude-3-5-sonnet");
        assert_eq!(result.tier, ModelTier::Mid);

        let result = classify_model("north/mini-code");
        assert_eq!(result.tier, ModelTier::Small);
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
