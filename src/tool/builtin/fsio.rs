//! Shared file IO helpers for the built-in file tools: atomic writes, stale-read
//! detection and line-ending / BOM preservation.

use crate::tool::tool_trait::ToolError;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Reads larger than this are refused so a huge file cannot exhaust memory.
pub const MAX_FILE_BYTES: u64 = 20 * 1024 * 1024;

static STAMPS: Mutex<Option<HashMap<PathBuf, Vec<u8>>>> = Mutex::new(None);

fn key(path: &Path) -> PathBuf {
    std::fs::canonicalize(path)
        .or_else(|_| std::path::absolute(path))
        .unwrap_or_else(|_| path.to_path_buf())
}

fn digest(bytes: &[u8]) -> Vec<u8> {
    Sha256::digest(bytes).as_slice().to_vec()
}

/// Remember the content of `path` as last seen by the model.
pub fn record(path: &Path, bytes: &[u8]) {
    let mut guard = STAMPS.lock().unwrap_or_else(|e| e.into_inner());
    guard
        .get_or_insert_with(HashMap::new)
        .insert(key(path), digest(bytes));
}

/// Fail if the model previously saw `path` and its content has since changed
/// (another tool, a subagent or the user). Files never read are not checked.
pub fn check_unchanged(path: &Path, current: &[u8]) -> Result<(), ToolError> {
    let guard = STAMPS.lock().unwrap_or_else(|e| e.into_inner());
    match guard.as_ref().and_then(|m| m.get(&key(path))) {
        Some(seen) if *seen != digest(current) => Err(ToolError::ExecutionFailed(format!(
            "'{}' changed since you last read it; read it again before modifying it",
            path.display()
        ))),
        _ => Ok(()),
    }
}

/// Read a file as UTF-8 text with a size cap. Missing files yield `None`.
pub fn read_existing(path: &Path) -> Result<Option<Vec<u8>>, ToolError> {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(ToolError::ExecutionFailed(format!(
                "failed to read '{}': {e}",
                path.display()
            )));
        }
    };
    if meta.len() > MAX_FILE_BYTES {
        return Err(ToolError::ExecutionFailed(format!(
            "'{}' is too large ({} bytes, limit {MAX_FILE_BYTES})",
            path.display(),
            meta.len()
        )));
    }
    std::fs::read(path).map(Some).map_err(|e| {
        ToolError::ExecutionFailed(format!("failed to read '{}': {e}", path.display()))
    })
}

/// Write `bytes` to `path` atomically: a sibling temp file is fsynced and
/// renamed over the target, so a crash never leaves a truncated file.
/// Symlinks are written through to their target.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let dir = target.parent().unwrap_or_else(|| Path::new("."));
    let tmp = dir.join(format!(".okeanos-tmp-{}", uuid::Uuid::new_v4().simple()));

    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        if let Ok(meta) = std::fs::metadata(&target) {
            let _ = std::fs::set_permissions(&tmp, meta.permissions());
        }
        std::fs::rename(&tmp, &target)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

const BOM: &str = "\u{feff}";

/// Text plus the formatting details that must survive an edit.
pub struct TextFormat {
    pub bom: bool,
    pub crlf: bool,
}

/// Decode `bytes` as UTF-8, normalising a BOM and uniform CRLF endings so
/// edits match regardless of platform. Returns the normalised text and the
/// format to restore with [`TextFormat::restore`].
pub fn decode(path: &Path, bytes: Vec<u8>) -> Result<(String, TextFormat), ToolError> {
    let text = String::from_utf8(bytes).map_err(|_| {
        ToolError::ExecutionFailed(format!(
            "'{}' is not valid UTF-8; refusing to modify it to avoid corrupting it",
            path.display()
        ))
    })?;
    let (bom, text) = match text.strip_prefix(BOM) {
        Some(rest) => (true, rest.to_string()),
        None => (false, text),
    };
    // Only treat the file as CRLF when no bare LF exists, so mixed files are untouched.
    let crlf = text.contains("\r\n") && !text.replace("\r\n", "").contains('\n');
    let text = if crlf {
        text.replace("\r\n", "\n")
    } else {
        text
    };
    Ok((text, TextFormat { bom, crlf }))
}

impl TextFormat {
    pub fn plain() -> Self {
        Self {
            bom: false,
            crlf: false,
        }
    }

    /// Re-apply the original BOM and line endings.
    pub fn restore(&self, text: &str) -> Vec<u8> {
        let body = if self.crlf {
            text.replace("\r\n", "\n").replace('\n', "\r\n")
        } else {
            text.to_string()
        };
        let mut out = Vec::with_capacity(body.len() + 3);
        if self.bom {
            out.extend_from_slice(BOM.as_bytes());
        }
        out.extend_from_slice(body.as_bytes());
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_replaces_and_leaves_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.txt");
        std::fs::write(&p, "old").unwrap();
        atomic_write(&p, b"new").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "new");
        let leftovers = std::fs::read_dir(dir.path()).unwrap().count();
        assert_eq!(leftovers, 1);
    }

    #[test]
    fn stale_read_is_detected() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("b.txt");
        std::fs::write(&p, "v1").unwrap();
        check_unchanged(&p, b"v1").unwrap();
        record(&p, b"v1");
        check_unchanged(&p, b"v1").unwrap();
        assert!(check_unchanged(&p, b"v2").is_err());
    }

    #[test]
    fn crlf_and_bom_round_trip() {
        let p = Path::new("x");
        let raw = "\u{feff}a\r\nb\r\n".as_bytes().to_vec();
        let (text, fmt) = decode(p, raw.clone()).unwrap();
        assert_eq!(text, "a\nb\n");
        assert_eq!(fmt.restore(&text), raw);
    }

    #[test]
    fn mixed_endings_untouched_and_invalid_utf8_rejected() {
        let p = Path::new("x");
        let (text, fmt) = decode(p, b"a\r\nb\n".to_vec()).unwrap();
        assert_eq!(text, "a\r\nb\n");
        assert!(!fmt.crlf);
        assert!(decode(p, vec![0xff, 0xfe, 0x00]).is_err());
    }
}
