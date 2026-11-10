//! Secret substitution for configuration values.
//!
//! Supports two reference forms inside config strings:
//! - `{env:VAR}`  → value of environment variable `VAR`
//! - `{file:path}` → trimmed contents of the file at `path`
//!
//! A missing or unreadable secret fails the substitution (fail closed).

use crate::config::ConfigError;

/// Substitute `{env:VAR}` and `{file:path}` references in `value`.
///
/// `key_path` is the dotted config path used in error messages so a missing
/// secret can be traced back to the config key that referenced it.
///
/// Segments that do not match either reference form are copied verbatim, so
/// literal braces in non-secret values are preserved.
pub fn substitute(value: &str, key_path: &str) -> Result<String, ConfigError> {
    let mut out = String::with_capacity(value.len());
    let mut i = 0;
    let bytes = value.as_bytes();

    while i < bytes.len() {
        if bytes[i] == b'{' {
            if let Some(close) = value[i..].find('}') {
                let inner = &value[i + 1..i + close];
                if let Some(replacement) = resolve_ref(inner, key_path)? {
                    out.push_str(&replacement);
                    i += close + 1;
                    continue;
                } // not a reference; fall through and copy '{'
            }
            out.push('{');
            i += 1;
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }

    Ok(out)
}

fn resolve_ref(inner: &str, key_path: &str) -> Result<Option<String>, ConfigError> {
    if let Some(var) = inner.strip_prefix("env:") {
        let var = var.trim();
        if var.is_empty() {
            return Err(ConfigError::UnsupportedSecretRef {
                key: key_path.to_string(),
                ref_: inner.to_string(),
            });
        }
        return match std::env::var(var) {
            Ok(value) => Ok(Some(value)),
            Err(std::env::VarError::NotPresent) | Err(std::env::VarError::NotUnicode(_)) => {
                Err(ConfigError::MissingSecret {
                    key: key_path.to_string(),
                    secret: var.to_string(),
                })
            }
        };
    }

    if let Some(path) = inner.strip_prefix("file:") {
        let path = path.trim();
        if path.is_empty() {
            return Err(ConfigError::UnsupportedSecretRef {
                key: key_path.to_string(),
                ref_: inner.to_string(),
            });
        }
        let content = std::fs::read_to_string(path).map_err(|e| ConfigError::SecretFileRead {
            path: path.to_string(),
            source: e,
        })?;
        let trimmed = content.trim_end().to_string();
        if trimmed.is_empty() {
            return Err(ConfigError::SecretFileEmpty {
                path: path.to_string(),
            });
        }
        return Ok(Some(trimmed));
    }

    Ok(None)
}
