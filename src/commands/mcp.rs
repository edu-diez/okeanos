use crate::prelude::*;

/// `okeanos mcp login|logout <server>`.
#[cfg(feature = "mcp")]
pub(crate) async fn cmd_mcp(cli: &Cli, action: &okeanos::cli::McpAction) -> anyhow::Result<()> {
    use okeanos::cli::McpAction;
    use okeanos::tool::mcp::oauth;

    let config = Config::load_raw(cli.root.as_deref()).context("failed to load configuration")?;
    let servers = resolve_mcp_client_config(&config).servers;
    let name = match action {
        McpAction::Login { server } | McpAction::Logout { server } => server,
    };
    let Some(server) = servers.iter().find(|s| &s.name == name) else {
        let known: Vec<&str> = servers
            .iter()
            .filter(|s| s.url.is_some())
            .map(|s| s.name.as_str())
            .collect();
        anyhow::bail!(
            "unknown MCP server '{name}' (HTTP servers: {})",
            if known.is_empty() {
                "none configured".to_string()
            } else {
                known.join(", ")
            }
        );
    };
    anyhow::ensure!(
        server.url.is_some(),
        "MCP server '{name}' is a stdio server; OAuth applies to Streamable HTTP servers (`url`)"
    );
    match action {
        McpAction::Login { .. } => {
            oauth::login(server, |url| {
                println!("Opening your browser to authorize '{name}'. If it does not open, visit:");
                println!("{url}");
                oauth::open_browser(url);
            })
            .await?;
            println!(
                "Logged in to '{name}'. Credentials are stored outside the config (0600 on Unix)."
            );
        }
        McpAction::Logout { .. } => {
            if oauth::logout(server)? {
                println!("Removed stored credentials for '{name}'.");
            } else {
                println!("No stored credentials for '{name}'.");
            }
        }
    }
    Ok(())
}

#[cfg(not(feature = "mcp"))]
pub(crate) async fn cmd_mcp(_cli: &Cli, _action: &okeanos::cli::McpAction) -> anyhow::Result<()> {
    anyhow::bail!("this build was compiled without the `mcp` feature")
}
