use crate::prelude::*;

pub(crate) fn cmd_trust(cli: &Cli, revoke: bool) -> anyhow::Result<()> {
    use okeanos::config::trust;
    let dir = trust::project_dir(cli.root.as_deref());
    if revoke {
        if trust::revoke(&dir).map_err(anyhow::Error::msg)? {
            println!("Revoked trust for {}", dir.display());
        } else {
            println!("{} was not trusted", dir.display());
        }
        return Ok(());
    }
    let files = trust::project_files(&dir);
    if files.is_empty() {
        println!("No project config files found in {}", dir.display());
        return Ok(());
    }
    trust::trust(&dir).map_err(anyhow::Error::msg)?;
    println!("Trusted {}:", dir.display());
    for f in files {
        println!("  {}", f.display());
    }
    println!("Trust is re-checked whenever these files change.");
    Ok(())
}

/// Warn about (or, on a terminal, offer to trust) an untrusted project config.
/// Returns true when the config should be reloaded because trust was granted.
pub(crate) fn handle_untrusted_project(cli: &Cli, config: &Config) -> bool {
    use std::io::{IsTerminal, Write};
    let okeanos::config::trust::ProjectTrust::Untrusted { files } = &config.project_trust else {
        return false;
    };
    let dir = okeanos::config::trust::project_dir(cli.root.as_deref());
    eprintln!("This project has configuration that can run code or reach the network:");
    for f in files {
        eprintln!("  {}", f.display());
    }
    if cli.mode() == Mode::Interactive && std::io::stdin().is_terminal() {
        eprint!("Trust it? Hooks, MCP servers and custom agents will be enabled. [y/N] ");
        let _ = std::io::stderr().flush();
        let mut answer = String::new();
        if std::io::stdin().read_line(&mut answer).is_ok()
            && matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
            && okeanos::config::trust::trust(&dir).is_ok()
        {
            return true;
        }
    }
    eprintln!(
        "Running restricted: hooks, MCP servers, providers, custom agents and allow rules from \
         the project are ignored. Run `okeanos trust` to enable them."
    );
    false
}
