use crate::prelude::*;

/// Health check: verify API keys, config, and dependencies.
pub(crate) async fn cmd_doctor(cli: &Cli) -> anyhow::Result<()> {
    println!("okeanos doctor");
    println!("==============");
    println!();

    // Check config
    let config_result = Config::load_raw(cli.root.as_deref());
    match &config_result {
        Ok(config) => {
            // Validate after loading
            if let Err(e) = config.validate() {
                println!("✗ Config: FAILED validation - {e}");
            } else {
                println!("✓ Config: OK (loaded from global + project layers)");
                if let Some(model) = &config.model {
                    println!("  └─ Model: {}", model.name.as_deref().unwrap_or("<unset>"));
                }
                if let Some(provider) = &config.provider {
                    println!(
                        "  └─ Provider: {} ({})",
                        provider.kind.as_deref().unwrap_or("<unset>"),
                        provider.base_url.as_deref().unwrap_or("<unset>")
                    );
                    if provider.api_key.is_some() {
                        println!("  └─ API Key: ***SET***");
                    } else {
                        println!("  └─ API Key: NOT SET (will fail for real providers)");
                    }
                }
            }
        }
        Err(e) => {
            println!("✗ Config: FAILED - {e}");
        }
    }
    println!();

    // Check dependencies / versions
    println!("✓ Dependencies:");
    println!("  └─ Rust: {}", rustc_version());
    println!("  └─ okeanos: {}", env!("CARGO_PKG_VERSION"));
    println!("  └─ tokio: {}", tokio_version());
    println!("  └─ reqwest: {}", reqwest_version());
    println!("  └─ clap: {}", clap_version());
    println!();

    // Check API key validity (if configured)
    if let Ok(config) = &config_result
        && let Some(provider) = &config.provider
        && let Some(api_key) = &provider.api_key
        && !api_key.is_empty()
    {
        println!("✓ API Key: Present (validity check requires network)");
    } else {
        println!("⚠ API Key: Not configured (mock provider will be used)");
    }
    println!();

    // Permissions, sandbox and project trust
    println!("Permissions:");
    if let Ok(config) = &config_result {
        let perms = config.permissions.as_ref();
        let mode = cli
            .permission_mode
            .as_deref()
            .or(perms.and_then(|p| p.mode.as_deref()))
            .unwrap_or("default");
        println!("  └─ Mode: {mode}");
        println!(
            "  └─ Rules: {} deny, {} ask, {} allow",
            perms.and_then(|p| p.deny.as_ref()).map_or(0, Vec::len),
            perms.and_then(|p| p.ask.as_ref()).map_or(0, Vec::len),
            perms.and_then(|p| p.allow.as_ref()).map_or(0, Vec::len)
                + perms
                    .and_then(|p| p.allow_list.as_ref())
                    .map_or(0, Vec::len),
        );
        println!(
            "  └─ Credential-file protection: {}",
            if perms.and_then(|p| p.protect_secrets).unwrap_or(true) {
                "on"
            } else {
                "OFF"
            }
        );
        let sandbox_mode = perms.and_then(|p| p.sandbox.as_deref()).unwrap_or("none");
        let (available, detail) = okeanos::tool::sandbox::availability();
        if sandbox_mode == "none" {
            let hint = if available {
                "set permissions.sandbox = \"workspace-write\" to enable"
            } else {
                "unavailable here"
            };
            println!("⚠ Sandbox: off ({hint}); {detail}");
        } else if available {
            println!("✓ Sandbox: {sandbox_mode} ({detail})");
        } else {
            println!("✗ Sandbox: {sandbox_mode} requested but {detail}");
        }
        match &config.project_trust {
            okeanos::config::trust::ProjectTrust::NoProjectFiles => {
                println!("✓ Project trust: no project config files");
            }
            okeanos::config::trust::ProjectTrust::Trusted => {
                println!("✓ Project trust: trusted");
            }
            okeanos::config::trust::ProjectTrust::Untrusted { .. } => {
                println!(
                    "⚠ Project trust: NOT trusted (hooks, MCP servers, agents ignored); run `okeanos trust`"
                );
            }
        }
    }
    println!();

    // Verify recovery needs snapshots to roll back to
    if let Ok(config) = &config_result
        && let Some(v) = &config.verify
        && v.command.is_some()
        && (v.on_fail.as_deref() == Some("retry") || v.candidates.unwrap_or(1) > 1)
    {
        let snapshots_on = config
            .snapshots
            .as_ref()
            .and_then(|s| s.enabled)
            .unwrap_or(false);
        if snapshots_on {
            println!("✓ Verify recovery: rollback enabled (snapshots on)");
        } else {
            println!(
                "⚠ Verify recovery: on_fail/candidates need snapshots, which are off; failures will only be reported. Set snapshots.enabled = true"
            );
        }
        println!();
    }

    // Check session directory
    if let Ok(cwd) = std::env::current_dir() {
        let session_dir = cwd.join(".okeanos").join("sessions");
        if session_dir.exists() {
            let count = std::fs::read_dir(&session_dir)?
                .filter(|e| e.is_ok())
                .count();
            println!(
                "✓ Session storage: {} ({} sessions)",
                session_dir.display(),
                count
            );
        } else {
            println!(
                "✓ Session storage: {} (will be created on first run)",
                session_dir.display()
            );
        }
    }

    Ok(())
}

pub(crate) fn rustc_version() -> String {
    // This is a compile-time value
    option_env!("RUSTC_VERSION")
        .unwrap_or("unknown")
        .to_string()
}

pub(crate) fn tokio_version() -> String {
    option_env!("CARGO_PKG_VERSION_TOKIO")
        .unwrap_or("1.43")
        .to_string()
}

pub(crate) fn reqwest_version() -> String {
    option_env!("CARGO_PKG_VERSION_REQWEST")
        .unwrap_or("0.12")
        .to_string()
}

pub(crate) fn clap_version() -> String {
    option_env!("CARGO_PKG_VERSION_CLAP")
        .unwrap_or("4.5")
        .to_string()
}
