//! Local model cache with TTL-based expiration.

use crate::model::discovery::DiscoveredModel;
use chrono::{DateTime, Duration, Utc};
use dirs;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::PathBuf;
use thiserror::Error;

/// Cache file name.
const CACHE_FILE_NAME: &str = "models.json";

/// Default TTL for cached models (24 hours).
const DEFAULT_TTL_HOURS: i64 = 24;

/// Errors during cache operations.
#[derive(Debug, Error)]
pub enum CacheError {
    #[error("IO error: {0}")]
    Io(#[from] io::Error),
    #[error("JSON serialization error: {0}")]
    SerdeJson(#[from] serde_json::Error),
    #[error("Cache directory not found")]
    CacheDirNotFound,
}

/// Cached model entry with timestamp.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedModelEntry {
    model: DiscoveredModel,
    cached_at: DateTime<Utc>,
}

/// Cache payload structure.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachePayload {
    models: HashMap<String, CachedModelEntry>,
    fetched_at: DateTime<Utc>,
}

/// Model cache with TTL-based expiration.
#[derive(Debug, Clone)]
pub struct ModelCache {
    cache_dir: PathBuf,
    ttl_hours: i64,
}

impl ModelCache {
    /// Create a new model cache with default TTL (24 hours).
    pub fn new() -> Result<Self, CacheError> {
        let cache_dir = get_cache_dir()?;
        Ok(Self {
            cache_dir,
            ttl_hours: DEFAULT_TTL_HOURS,
        })
    }

    /// Create a new model cache with custom TTL.
    pub fn with_ttl(ttl_hours: i64) -> Result<Self, CacheError> {
        let cache_dir = get_cache_dir()?;
        Ok(Self {
            cache_dir,
            ttl_hours,
        })
    }

    /// Get the cache file path.
    fn cache_file(&self) -> PathBuf {
        self.cache_dir.join(CACHE_FILE_NAME)
    }

    /// Check if cache exists and is fresh.
    pub fn is_fresh(&self) -> bool {
        if let Ok(payload) = self.load_payload() {
            let age = Utc::now() - payload.fetched_at;
            age < Duration::hours(self.ttl_hours)
        } else {
            false
        }
    }

    /// Get cached models if fresh, otherwise None.
    pub fn get(&self) -> Option<Vec<DiscoveredModel>> {
        if !self.is_fresh() {
            return None;
        }

        let payload = self.load_payload().ok()?;
        let models: Vec<DiscoveredModel> = payload
            .models
            .values()
            .map(|entry| entry.model.clone())
            .collect();

        if models.is_empty() {
            None
        } else {
            Some(models)
        }
    }

    /// Set models in cache with current timestamp.
    pub fn set(&self, models: Vec<DiscoveredModel>) -> Result<(), CacheError> {
        let mut entries = HashMap::new();
        let now = Utc::now();

        for model in models {
            entries.insert(
                model.id.clone(),
                CachedModelEntry {
                    model,
                    cached_at: now,
                },
            );
        }

        let payload = CachePayload {
            models: entries,
            fetched_at: now,
        };

        let json = serde_json::to_string_pretty(&payload)?;
        fs::write(self.cache_file(), json)?;

        Ok(())
    }

    /// Clear the cache.
    pub fn clear(&self) -> Result<(), CacheError> {
        let cache_file = self.cache_file();
        if cache_file.exists() {
            fs::remove_file(cache_file)?;
        }
        Ok(())
    }

    /// Load raw cache payload.
    fn load_payload(&self) -> Result<CachePayload, CacheError> {
        let cache_file = self.cache_file();
        if !cache_file.exists() {
            return Err(CacheError::CacheDirNotFound);
        }

        let json = fs::read_to_string(cache_file)?;
        let payload: CachePayload = serde_json::from_str(&json)?;
        Ok(payload)
    }

    /// Get cache stats (model count, age).
    pub fn stats(&self) -> Option<(usize, Duration)> {
        let payload = self.load_payload().ok()?;
        let age = Utc::now() - payload.fetched_at;
        Some((payload.models.len(), age))
    }
}

impl Default for ModelCache {
    fn default() -> Self {
        Self::new().expect("Failed to create default ModelCache")
    }
}

/// Get the cache directory for okeanos.
fn get_cache_dir() -> Result<PathBuf, CacheError> {
    let cache_dir = dirs::cache_dir()
        .ok_or(CacheError::CacheDirNotFound)?
        .join("okeanos");

    if !cache_dir.exists() {
        fs::create_dir_all(&cache_dir)?;
    }

    Ok(cache_dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ModelTier;
    use crate::model::discovery::{DiscoveredModel, ModelPricing, minimal_fallback_models};
    use tempfile::tempdir;

    fn create_test_model(id: &str, tier: ModelTier) -> DiscoveredModel {
        DiscoveredModel {
            id: id.to_string(),
            name: id.to_string(),
            context_length: 128_000,
            max_output_tokens: 8192,
            pricing: ModelPricing {
                prompt: "1.0".to_string(),
                completion: "1.0".to_string(),
                image: None,
                request: None,
            },
            supports_tools: true,
            supports_structured_output: true,
            description: None,
            tier,
        }
    }

    #[test]
    fn test_cache_set_and_get() {
        let dir = tempdir().unwrap();
        let cache = ModelCache {
            cache_dir: dir.path().to_path_buf(),
            ttl_hours: 24,
        };

        let models = vec![
            create_test_model("test-model-1", ModelTier::Frontier),
            create_test_model("test-model-2", ModelTier::Mid),
        ];

        cache.set(models.clone()).unwrap();
        let retrieved = cache.get().unwrap();

        assert_eq!(retrieved.len(), 2);
        let ids: std::collections::HashSet<_> = retrieved.iter().map(|m| m.id.clone()).collect();
        assert!(ids.contains("test-model-1"));
        assert!(ids.contains("test-model-2"));
    }

    #[test]
    fn test_cache_miss_when_empty() {
        let dir = tempdir().unwrap();
        let cache = ModelCache {
            cache_dir: dir.path().to_path_buf(),
            ttl_hours: 24,
        };

        assert!(cache.get().is_none());
        assert!(!cache.is_fresh());
    }

    #[test]
    fn test_cache_expiry() {
        let dir = tempdir().unwrap();
        let cache = ModelCache {
            cache_dir: dir.path().to_path_buf(),
            ttl_hours: 0, // Expired immediately
        };

        let models = vec![create_test_model("test-model", ModelTier::Small)];
        cache.set(models).unwrap();

        assert!(!cache.is_fresh());
        assert!(cache.get().is_none());
    }

    #[test]
    fn test_cache_clear() {
        let dir = tempdir().unwrap();
        let cache = ModelCache {
            cache_dir: dir.path().to_path_buf(),
            ttl_hours: 24,
        };

        let models = vec![create_test_model("test-model", ModelTier::Small)];
        cache.set(models).unwrap();
        assert!(cache.get().is_some());

        cache.clear().unwrap();
        assert!(cache.get().is_none());
    }

    #[test]
    fn test_minimal_fallback_models_cached() {
        let dir = tempdir().unwrap();
        let cache = ModelCache {
            cache_dir: dir.path().to_path_buf(),
            ttl_hours: 24,
        };

        let models = vec![
            create_test_model("test-frontier", ModelTier::Frontier),
            create_test_model("test-mid", ModelTier::Mid),
            create_test_model("test-small", ModelTier::Small),
        ];
        cache.set(models.clone()).unwrap();

        let retrieved = cache.get().unwrap();
        assert_eq!(retrieved.len(), 3);

        let ids: std::collections::HashSet<_> = retrieved.iter().map(|m| m.id.clone()).collect();
        assert!(ids.contains("test-frontier"));
        assert!(ids.contains("test-mid"));
        assert!(ids.contains("test-small"));
    }

    #[test]
    fn test_cache_stats() {
        let dir = tempdir().unwrap();
        let cache = ModelCache {
            cache_dir: dir.path().to_path_buf(),
            ttl_hours: 24,
        };

        assert!(cache.stats().is_none());

        let models = vec![create_test_model("test-model", ModelTier::Small)];
        cache.set(models).unwrap();

        let stats = cache.stats().unwrap();
        assert_eq!(stats.0, 1);
        assert!(stats.1 < Duration::seconds(1));
    }
}
