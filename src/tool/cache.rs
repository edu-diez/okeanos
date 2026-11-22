//! Tool result caching with TTL and invalidation.
//!
//! Provides optional caching for idempotent tool calls (read, grep).
//! Cache is disabled by default, configurable per-tool TTL, and invalidates
//! on write/delete/run operations affecting the same path.

use crate::tool::tool_trait::ToolExecutionResult;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Configuration for tool result caching.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CacheConfig {
    /// Enable tool result caching (default: false).
    pub enabled: bool,
    /// Default TTL in seconds for cacheable tools (default: 300 = 5 minutes).
    #[serde(default = "default_ttl")]
    pub default_ttl_secs: u64,
    /// Per-tool TTL overrides in seconds.
    /// e.g., { "read" = 600, "grep" = 300 }
    #[serde(default)]
    pub per_tool_ttl: HashMap<String, u64>,
    /// Maximum number of cache entries (default: 1000).
    #[serde(default = "default_max_entries")]
    pub max_entries: usize,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            default_ttl_secs: default_ttl(),
            per_tool_ttl: HashMap::new(),
            max_entries: default_max_entries(),
        }
    }
}

fn default_ttl() -> u64 {
    300 // 5 minutes
}

fn default_max_entries() -> usize {
    1000
}

impl CacheConfig {
    /// Check if a tool is cacheable.
    pub fn is_cacheable(&self, tool_name: &str) -> bool {
        self.enabled && matches!(tool_name, "read" | "grep")
    }

    /// Get TTL for a specific tool.
    pub fn ttl_for(&self, tool_name: &str) -> Duration {
        Duration::from_secs(
            self.per_tool_ttl
                .get(tool_name)
                .copied()
                .unwrap_or(self.default_ttl_secs)
                .max(1), // minimum 1 second
        )
    }
}

/// A cached tool result entry.
#[derive(Debug, Clone)]
struct CacheEntry {
    result: ToolExecutionResult,
    created_at: Instant,
    ttl: Duration,
    /// File paths and their hashes that this result depends on.
    /// Used for invalidation on file modification.
    file_hashes: HashMap<PathBuf, u64>,
}

impl CacheEntry {
    fn new(result: ToolExecutionResult, ttl: Duration, file_hashes: HashMap<PathBuf, u64>) -> Self {
        Self {
            result,
            created_at: Instant::now(),
            ttl,
            file_hashes,
        }
    }

    /// Check if the entry is still valid (not expired and files unchanged).
    fn is_valid(&self) -> bool {
        if self.created_at.elapsed() >= self.ttl {
            return false;
        }
        // Check if any tracked file has been modified
        for (path, &stored_hash) in &self.file_hashes {
            if let Ok(current_hash) = file_hash(path) {
                if current_hash != stored_hash {
                    return false;
                }
            } else {
                // File no longer exists or can't be read - invalidate
                return false;
            }
        }
        true
    }
}

/// Compute a simple hash of a file's contents + mtime for invalidation.
fn file_hash(path: &Path) -> Result<u64, std::io::Error> {
    use std::hash::{Hash, Hasher};
    let metadata = std::fs::metadata(path)?;
    let content = std::fs::read_to_string(path).unwrap_or_default();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    content.hash(&mut hasher);
    metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as u64)
        .hash(&mut hasher);
    Ok(hasher.finish())
}

/// Extract file paths from tool arguments for cache tracking and invalidation.
pub fn extract_paths(tool_name: &str, args: &serde_json::Value) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    match tool_name {
        "read" | "write" | "delete" => {
            if let Some(path) = args.get("path").and_then(|v| v.as_str()) {
                paths.push(PathBuf::from(path));
            }
        }
        "grep" => {
            if let Some(path) = args.get("path").and_then(|v| v.as_str()) {
                paths.push(PathBuf::from(path));
            }
        }
        "run" => {
            // run tool doesn't have a simple path to track; we could track
            // working directory but for now we just invalidate all on run
        }
        _ => {}
    }
    paths
}

/// Tool result cache for idempotent tool calls.
#[derive(Debug)]
pub struct ToolResultCache {
    entries: HashMap<String, CacheEntry>,
    config: CacheConfig,
}

impl ToolResultCache {
    /// Create a new cache with the given configuration.
    pub fn new(config: CacheConfig) -> Self {
        Self {
            entries: HashMap::new(),
            config,
        }
    }

    /// Get the cache configuration.
    pub fn config(&self) -> &CacheConfig {
        &self.config
    }

    /// Update the cache configuration.
    pub fn set_config(&mut self, config: CacheConfig) {
        self.config = config;
    }

    /// Generate a cache key from tool name and arguments.
    fn cache_key(&self, tool_name: &str, args: &serde_json::Value) -> String {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        tool_name.hash(&mut hasher);
        // Canonicalize args by serializing with sorted keys
        let canonical = serde_json::to_string(&canonicalize_value(args)).unwrap_or_default();
        canonical.hash(&mut hasher);
        format!("{:x}", hasher.finish())
    }

    /// Try to get a cached result for the given tool and arguments.
    ///
    /// Returns `Some(result)` if cache hit and valid, `None` otherwise.
    pub fn get(
        &mut self,
        tool_name: &str,
        args: &serde_json::Value,
    ) -> Option<ToolExecutionResult> {
        if !self.config.is_cacheable(tool_name) {
            return None;
        }

        let key = self.cache_key(tool_name, args);
        let entry = self.entries.get_mut(&key)?;

        if !entry.is_valid() {
            self.entries.remove(&key);
            return None;
        }

        Some(entry.result.clone())
    }

    /// Store a result in the cache.
    pub fn put(&mut self, tool_name: &str, args: &serde_json::Value, result: &ToolExecutionResult) {
        if !self.config.is_cacheable(tool_name) {
            return;
        }

        // Enforce max entries by removing oldest if needed
        if self.entries.len() >= self.config.max_entries {
            self.evict_oldest();
        }

        let paths = extract_paths(tool_name, args);
        let mut file_hashes = HashMap::new();
        for path in paths {
            if let Ok(hash) = file_hash(&path) {
                file_hashes.insert(path, hash);
            }
        }

        let ttl = self.config.ttl_for(tool_name);
        let key = self.cache_key(tool_name, args);
        self.entries
            .insert(key, CacheEntry::new(result.clone(), ttl, file_hashes));
    }

    /// Invalidate cache entries for a specific path.
    ///
    /// Called when a write/delete/run operation modifies a file.
    pub fn invalidate_path(&mut self, path: &Path) {
        self.entries
            .retain(|_, entry| !entry.file_hashes.contains_key(path));
    }

    /// Invalidate all cache entries.
    pub fn invalidate_all(&mut self) {
        self.entries.clear();
    }

    /// Remove the oldest cache entry (by creation time).
    fn evict_oldest(&mut self) {
        if let Some(oldest_key) = self
            .entries
            .iter()
            .min_by_key(|(_, e)| e.created_at)
            .map(|(k, _)| k.clone())
        {
            self.entries.remove(&oldest_key);
        }
    }

    /// Get current cache size.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Check if cache is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Canonicalize a JSON value by sorting object keys for consistent hashing.
fn canonicalize_value(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut sorted: Vec<_> = map.iter().collect();
            sorted.sort_by(|a, b| a.0.cmp(b.0));
            let mut result = serde_json::Map::new();
            for (k, v) in sorted {
                result.insert(k.clone(), canonicalize_value(v));
            }
            serde_json::Value::Object(result)
        }
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.iter().map(canonicalize_value).collect())
        }
        _ => value.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::tool_trait::ToolExecutionResult;
    use serde_json::json;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn test_cache_config_defaults() {
        let config = CacheConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.default_ttl_secs, 300);
        assert_eq!(config.max_entries, 1000);
    }

    #[test]
    fn test_cache_config_is_cacheable() {
        let mut config = CacheConfig::default();
        config.enabled = true;
        assert!(config.is_cacheable("read"));
        assert!(config.is_cacheable("grep"));
        assert!(!config.is_cacheable("write"));
        assert!(!config.is_cacheable("delete"));
        assert!(!config.is_cacheable("run"));
        assert!(!config.is_cacheable("todo"));

        config.enabled = false;
        assert!(!config.is_cacheable("read"));
    }

    #[test]
    fn test_cache_config_per_tool_ttl() {
        let mut config = CacheConfig::default();
        config.enabled = true;
        config.per_tool_ttl = HashMap::from([("read".to_string(), 600), ("grep".to_string(), 150)]);

        assert_eq!(config.ttl_for("read").as_secs(), 600);
        assert_eq!(config.ttl_for("grep").as_secs(), 150);
        assert_eq!(config.ttl_for("unknown").as_secs(), 300); // default
    }

    #[test]
    fn test_canonicalize_value() {
        let v1 = json!({"b": 2, "a": 1});
        let v2 = json!({"a": 1, "b": 2});
        assert_eq!(canonicalize_value(&v1), canonicalize_value(&v2));

        let v3 = json!({"outer": {"b": 2, "a": 1}});
        let v4 = json!({"outer": {"a": 1, "b": 2}});
        assert_eq!(canonicalize_value(&v3), canonicalize_value(&v4));

        let v5 = json!([{"b": 2, "a": 1}, 3]);
        let v6 = json!([{"a": 1, "b": 2}, 3]);
        assert_eq!(canonicalize_value(&v5), canonicalize_value(&v6));
    }

    #[test]
    fn test_cache_key_consistency() {
        let config = CacheConfig {
            enabled: true,
            ..Default::default()
        };
        let cache = ToolResultCache::new(config);

        let args1 = json!({"path": "/tmp/file.txt"});
        let args2 = json!({"path": "/tmp/file.txt"});
        assert_eq!(
            cache.cache_key("read", &args1),
            cache.cache_key("read", &args2)
        );

        // Different tool names produce different keys
        assert_ne!(
            cache.cache_key("read", &args1),
            cache.cache_key("grep", &args1)
        );
    }

    #[tokio::test]
    async fn test_cache_hit_miss() {
        let config = CacheConfig {
            enabled: true,
            ..Default::default()
        };
        let mut cache = ToolResultCache::new(config);

        let args = json!({"path": "/tmp/test.txt"});
        let result = ToolExecutionResult::output("file content");

        // Miss on empty cache
        assert!(cache.get("read", &args).is_none());

        // Put and hit
        cache.put("read", &args, &result);
        let cached = cache.get("read", &args);
        assert!(cached.is_some());
        assert_eq!(cached.unwrap().output, "file content");

        // Different args -> miss
        let args2 = json!({"path": "/tmp/other.txt"});
        assert!(cache.get("read", &args2).is_none());
    }

    #[tokio::test]
    async fn test_cache_ttl_expiry() {
        let config = CacheConfig {
            enabled: true,
            default_ttl_secs: 1, // 1 second TTL
            ..Default::default()
        };
        let mut cache = ToolResultCache::new(config);

        let args = json!({"path": "/tmp/test.txt"});
        let result = ToolExecutionResult::output("file content");

        cache.put("read", &args, &result);
        assert!(cache.get("read", &args).is_some());

        // Wait for TTL to expire
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(cache.get("read", &args).is_none());
    }

    #[tokio::test]
    async fn test_cache_invalidate_path() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        fs::write(&file_path, "original content").unwrap();

        let config = CacheConfig {
            enabled: true,
            ..Default::default()
        };
        let mut cache = ToolResultCache::new(config);

        let args = json!({"path": file_path.to_str().unwrap()});
        let result = ToolExecutionResult::output("original content");

        cache.put("read", &args, &result);
        assert!(cache.get("read", &args).is_some());

        // Invalidate the path
        cache.invalidate_path(&file_path);
        assert!(cache.get("read", &args).is_none());
    }

    #[tokio::test]
    async fn test_cache_file_modification_invalidates() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        fs::write(&file_path, "original content").unwrap();

        let config = CacheConfig {
            enabled: true,
            default_ttl_secs: 60,
            ..Default::default()
        };
        let mut cache = ToolResultCache::new(config);

        let args = json!({"path": file_path.to_str().unwrap()});
        let result = ToolExecutionResult::output("original content");

        cache.put("read", &args, &result);
        assert!(cache.get("read", &args).is_some());

        // Modify the file
        tokio::time::sleep(Duration::from_millis(100)).await; // Ensure mtime changes
        fs::write(&file_path, "modified content").unwrap();

        // Cache should be invalidated on next get
        assert!(cache.get("read", &args).is_none());
    }

    #[tokio::test]
    async fn test_cache_non_cacheable_tools() {
        let config = CacheConfig {
            enabled: true,
            ..Default::default()
        };
        let mut cache = ToolResultCache::new(config);

        let args = json!({"path": "/tmp/test.txt", "content": "hello"});
        let result = ToolExecutionResult::output("written");

        // write tool should not be cached
        cache.put("write", &args, &result);
        assert!(cache.get("write", &args).is_none());

        // delete tool should not be cached
        let delete_args = json!({"path": "/tmp/test.txt"});
        cache.put("delete", &delete_args, &result);
        assert!(cache.get("delete", &delete_args).is_none());

        // run tool should not be cached
        let run_args = json!({"command": "echo", "args": ["hello"]});
        cache.put("run", &run_args, &result);
        assert!(cache.get("run", &run_args).is_none());
    }

    #[tokio::test]
    async fn test_cache_disabled_by_default() {
        let config = CacheConfig::default();
        let mut cache = ToolResultCache::new(config);

        let args = json!({"path": "/tmp/test.txt"});
        let result = ToolExecutionResult::output("file content");

        cache.put("read", &args, &result);
        assert!(cache.get("read", &args).is_none());
    }

    #[tokio::test]
    async fn test_cache_max_entries_eviction() {
        let config = CacheConfig {
            enabled: true,
            max_entries: 2,
            ..Default::default()
        };
        let mut cache = ToolResultCache::new(config);

        let result = ToolExecutionResult::output("content");

        cache.put("read", &json!({"path": "/tmp/a.txt"}), &result);
        cache.put("read", &json!({"path": "/tmp/b.txt"}), &result);
        assert_eq!(cache.len(), 2);

        // Adding a third should evict the oldest
        cache.put("read", &json!({"path": "/tmp/c.txt"}), &result);
        assert_eq!(cache.len(), 2);
    }

    #[tokio::test]
    async fn test_cache_invalidate_all() {
        let config = CacheConfig {
            enabled: true,
            ..Default::default()
        };
        let mut cache = ToolResultCache::new(config);

        let result = ToolExecutionResult::output("content");
        cache.put("read", &json!({"path": "/tmp/a.txt"}), &result);
        cache.put("read", &json!({"path": "/tmp/b.txt"}), &result);
        cache.put("grep", &json!({"pattern": "foo", "path": "/tmp"}), &result);

        assert_eq!(cache.len(), 3);
        cache.invalidate_all();
        assert_eq!(cache.len(), 0);
    }

    #[tokio::test]
    async fn test_extract_paths() {
        let read_args = json!({"path": "/tmp/file.txt"});
        assert_eq!(
            extract_paths("read", &read_args),
            vec![PathBuf::from("/tmp/file.txt")]
        );

        let write_args = json!({"path": "/tmp/file.txt", "content": "hi"});
        assert_eq!(
            extract_paths("write", &write_args),
            vec![PathBuf::from("/tmp/file.txt")]
        );

        let grep_args = json!({"pattern": "foo", "path": "/tmp/dir"});
        assert_eq!(
            extract_paths("grep", &grep_args),
            vec![PathBuf::from("/tmp/dir")]
        );

        let run_args = json!({"command": "echo", "args": ["hi"]});
        assert_eq!(extract_paths("run", &run_args), Vec::<PathBuf>::new());
    }
}
