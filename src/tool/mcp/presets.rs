//! Built-in remote MCP servers that are on by default: no login, read-only
//! documentation and code search. Opt out with `[mcp] builtin_servers = false`
//! or override/disable one by defining a server with the same name.

use super::client::McpServerConfig;

/// Connect budget for presets so an offline start is never slow.
const PRESET_TIMEOUT_SECS: u64 = 8;

/// `(name, endpoint)` of each built-in server.
const PRESETS: &[(&str, &str)] = &[
    // Library/framework documentation lookup.
    ("context7", "https://mcp.context7.com/mcp"),
    // Fast code search across public GitHub repositories (grep.app).
    ("gh_grep", "https://mcp.grep.app"),
];

/// Built-in servers not already defined by the user (by name).
pub fn builtin_servers(existing: &[McpServerConfig]) -> Vec<McpServerConfig> {
    PRESETS
        .iter()
        .filter(|(name, _)| !existing.iter().any(|s| s.name == *name))
        .map(|(name, url)| McpServerConfig {
            name: (*name).to_string(),
            url: Some((*url).to_string()),
            enabled: true,
            timeout_secs: PRESET_TIMEOUT_SECS,
            read_only: true,
            ..Default::default()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_are_read_only_http_servers() {
        let servers = builtin_servers(&[]);
        assert_eq!(servers.len(), PRESETS.len());
        assert!(
            servers
                .iter()
                .all(|s| s.read_only && s.url.is_some() && s.enabled)
        );
    }

    #[test]
    fn user_definition_overrides_preset() {
        let user = McpServerConfig {
            name: "context7".into(),
            enabled: false,
            ..Default::default()
        };
        let servers = builtin_servers(&[user]);
        assert!(servers.iter().all(|s| s.name != "context7"));
        assert!(servers.iter().any(|s| s.name == "gh_grep"));
    }
}
