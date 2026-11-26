//! Best-effort secret scrubbing for persisted logs.
//!
//! Applied to each serialized session-log line. Replacements never contain
//! quotes or backslashes, so a scrubbed line is still valid JSON.

use once_cell::sync::Lazy;
use regex::{Regex, RegexSet};
use std::borrow::Cow;

const REDACTED: &str = "[REDACTED]";

/// (pattern, replacement). Patterns with a capture group keep group 1.
const RULES: &[(&str, &str)] = &[
    (r"sk-[A-Za-z0-9_\-]{16,}", REDACTED),
    (r"gh[pousr]_[A-Za-z0-9]{20,}", REDACTED),
    (r"github_pat_[A-Za-z0-9_]{20,}", REDACTED),
    (r"AKIA[0-9A-Z]{16}", REDACTED),
    (r"AIza[0-9A-Za-z_\-]{30,}", REDACTED),
    (r"xox[baprs]-[A-Za-z0-9\-]{10,}", REDACTED),
    (
        r"eyJ[A-Za-z0-9_\-]{8,}\.[A-Za-z0-9_\-]{8,}\.[A-Za-z0-9_\-]{8,}",
        REDACTED,
    ),
    (r"(?i)(bearer\s+)[A-Za-z0-9._~+/=\-]{16,}", "${1}[REDACTED]"),
    (
        r#"(?i)((?:api[_-]?key|secret|token|passw(?:or)?d|authorization)[A-Za-z_]*\\?"?\s*[:=]\s*\\?"?)[A-Za-z0-9_\-./+=]{12,}"#,
        "${1}[REDACTED]",
    ),
];

static SET: Lazy<RegexSet> =
    Lazy::new(|| RegexSet::new(RULES.iter().map(|(p, _)| *p)).expect("valid redaction patterns"));

static REGEXES: Lazy<Vec<Regex>> = Lazy::new(|| {
    RULES
        .iter()
        .map(|(p, _)| Regex::new(p).expect("valid redaction pattern"))
        .collect()
});

/// Whether log redaction is enabled (`OKEANOS_LOG_REDACT=0` disables it).
pub fn enabled() -> bool {
    !matches!(
        std::env::var("OKEANOS_LOG_REDACT").as_deref(),
        Ok("0") | Ok("false")
    )
}

/// Replace likely secrets (API keys, bearer tokens, `password=...`) in `input`.
pub fn scrub(input: &str) -> Cow<'_, str> {
    let hits = SET.matches(input);
    if !hits.matched_any() {
        return Cow::Borrowed(input);
    }
    let mut out = input.to_string();
    for idx in hits.iter() {
        out = REGEXES[idx].replace_all(&out, RULES[idx].1).into_owned();
    }
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaves_clean_text_untouched() {
        let s = r#"{"output":"hello world","tokens_in":1234}"#;
        assert!(matches!(scrub(s), Cow::Borrowed(_)));
    }

    #[test]
    fn scrubs_known_key_shapes() {
        let s = "key sk-ant-api03-abcdefghijklmnop1234 and ghp_abcdefghijklmnopqrstuv1234";
        let out = scrub(s);
        assert!(!out.contains("abcdefghijklmnop1234"));
        assert!(!out.contains("ghp_abcdef"));
    }

    #[test]
    fn scrubs_assignments_inside_escaped_json() {
        let s = r#"{"output":"API_KEY=\"supersecretvalue123\"\nOTHER=1"}"#;
        let out = scrub(s);
        assert!(!out.contains("supersecretvalue123"));
        assert!(serde_json::from_str::<serde_json::Value>(&out).is_ok());
    }

    #[test]
    fn scrubs_bearer_but_keeps_prefix() {
        let out = scrub("Authorization: Bearer abcdefghijklmnopqrstuvwxyz");
        assert!(!out.contains("abcdefghijklmnopqrstuvwxyz"));
    }
}
