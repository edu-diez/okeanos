//! Learned per-model tier adjustments (M39e).
//!
//! When a model's tool-call validity stays low, the loop lowers its effective
//! tier and records it here so the next session starts at that tier. Only
//! downgrades are ever stored; explicit config always wins at the call site.

use crate::model::ModelTier;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const FILE_NAME: &str = "model_profiles.json";

/// Default location: `<cache dir>/okeanos/model_profiles.json`.
pub fn default_path() -> Option<PathBuf> {
    Some(dirs::cache_dir()?.join("okeanos").join(FILE_NAME))
}

fn read(path: &Path) -> BTreeMap<String, ModelTier> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// Stored tier for `model_id`, if any.
pub fn load_from(path: &Path, model_id: &str) -> Option<ModelTier> {
    read(path).get(model_id).copied()
}

/// Remember `tier` for `model_id`; keeps the lower tier if one is already stored.
pub fn save_to(path: &Path, model_id: &str, tier: ModelTier) -> std::io::Result<()> {
    let mut profiles = read(path);
    let keep = profiles
        .get(model_id)
        .is_some_and(|old| old.rank() <= tier.rank());
    if keep {
        return Ok(());
    }
    profiles.insert(model_id.to_string(), tier);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text = serde_json::to_string_pretty(&profiles).map_err(std::io::Error::other)?;
    crate::tool::builtin::fsio::atomic_write(path, text.as_bytes())
}

/// [`load_from`] at the default path.
pub fn load(model_id: &str) -> Option<ModelTier> {
    load_from(&default_path()?, model_id)
}

/// [`save_to`] at the default path.
pub fn save(model_id: &str, tier: ModelTier) -> std::io::Result<()> {
    match default_path() {
        Some(p) => save_to(&p, model_id, tier),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_only_lowers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.json");
        assert_eq!(load_from(&path, "m"), None);
        save_to(&path, "m", ModelTier::Mid).unwrap();
        assert_eq!(load_from(&path, "m"), Some(ModelTier::Mid));
        save_to(&path, "m", ModelTier::Frontier).unwrap();
        assert_eq!(load_from(&path, "m"), Some(ModelTier::Mid));
        save_to(&path, "m", ModelTier::Small).unwrap();
        assert_eq!(load_from(&path, "m"), Some(ModelTier::Small));
    }
}
