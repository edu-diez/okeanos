//! Loader for the portable `.mcp.json` project file (`{"mcpServers": {...}}`),
//! so MCP servers configured for other agents work here unchanged.

use std::path::Path;

use serde::Deserialize;
use std::collections::HashMap;

use super::client::McpServerConfig;

#[derive(Deserialize)]
struct McpJson {
    #[serde(default, rename = "mcpServers")]
    servers: HashMap<String, JsonServer>,
}

#[derive(Deserialize, Default)]
struct JsonServer {
    #[serde(default)]
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: HashMap<String, String>,
    #[serde(default)]
    cwd: Option<std::path::PathBuf>,
    #[serde(default)]
    url: Option<String>,
    /// `Authorization: Bearer <token>` is extracted from here when present.
    #[serde(default)]
    headers: HashMap<String, String>,
    #[serde(default)]
    disabled: bool,
}

/// Expand `${VAR}` and `${VAR:-default}` using the process environment.
fn expand(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            out.push_str(&rest[start..]);
            return out;
        };
        let (name, default) = match after[..end].split_once(":-") {
            Some((n, d)) => (n, Some(d)),
            None => (&after[..end], None),
        };
        match std::env::var(name) {
            Ok(v) => out.push_str(&v),
            Err(_) => out.push_str(default.unwrap_or("")),
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

/// Parse `.mcp.json` content. Servers are returned sorted by name.
pub fn parse_mcp_json(text: &str) -> Result<Vec<McpServerConfig>, serde_json::Error> {
    let parsed: McpJson = serde_json::from_str(text)?;
    let mut servers: Vec<McpServerConfig> = parsed
        .servers
        .into_iter()
        .map(|(name, s)| {
            let bearer_token = s
                .headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
                .map(|(_, v)| expand(v))
                .map(|v| v.strip_prefix("Bearer ").map(str::to_string).unwrap_or(v));
            McpServerConfig {
                name,
                command: expand(&s.command),
                args: s.args.iter().map(|a| expand(a)).collect(),
                env: s.env.iter().map(|(k, v)| (k.clone(), expand(v))).collect(),
                cwd: s.cwd,
                enabled: !s.disabled,
                timeout_secs: 30,
                url: s.url.map(|u| expand(&u)),
                bearer_token,
                read_only: false,
            }
        })
        .collect();
    servers.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(servers)
}

/// Load `<dir>/.mcp.json` if it exists. Parse errors are reported, not fatal.
pub fn load_project_mcp_json(dir: &Path) -> Vec<McpServerConfig> {
    let path = dir.join(".mcp.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    match parse_mcp_json(&text) {
        Ok(servers) => servers,
        Err(e) => {
            tracing::warn!("ignoring {}: {e}", path.display());
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stdio_and_http_servers() {
        // SAFETY: test-only env var with a unique name.
        unsafe { std::env::set_var("OKEANOS_MCPJSON_TEST", "tok") };
        let servers = parse_mcp_json(
            r#"{"mcpServers": {
                "fs": {"command": "npx", "args": ["-y", "srv", "${OKEANOS_MCPJSON_TEST}"]},
                "web": {"url": "https://x.test/mcp",
                        "headers": {"Authorization": "Bearer ${OKEANOS_MCPJSON_TEST}"}},
                "off": {"command": "x", "disabled": true}
            }}"#,
        )
        .unwrap();
        assert_eq!(servers.len(), 3);
        let fs = servers.iter().find(|s| s.name == "fs").unwrap();
        assert_eq!(fs.args, ["-y", "srv", "tok"]);
        let web = servers.iter().find(|s| s.name == "web").unwrap();
        assert_eq!(web.url.as_deref(), Some("https://x.test/mcp"));
        assert_eq!(web.bearer_token.as_deref(), Some("tok"));
        assert!(!servers.iter().find(|s| s.name == "off").unwrap().enabled);
    }

    #[test]
    fn expand_uses_default() {
        assert_eq!(expand("a${OKEANOS_UNSET_XYZ:-b}c"), "abc");
    }
}
