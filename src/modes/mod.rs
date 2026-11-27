//! Run modes: interactive TUI, headless, ACP and JSON-RPC (stdio / WebSocket).

mod acp;
mod headless;
mod interactive;
mod rpc;
mod rpc_ws;

use crate::prelude::*;

pub(crate) use acp::run_acp;
#[cfg(feature = "rpc")]
pub(crate) use headless::handle_headless_approvals;
pub(crate) use headless::run_headless;
pub(crate) use interactive::run_interactive;
pub(crate) use rpc::run_rpc;
pub(crate) use rpc_ws::run_rpc_ws;

pub(crate) async fn run_with_prompt(cli: &Cli, prompt: Option<String>) -> anyhow::Result<()> {
    let mut config =
        Config::load_raw(cli.root.as_deref()).context("failed to load configuration")?;
    if handle_untrusted_project(cli, &config) {
        config = Config::load_raw(cli.root.as_deref()).context("failed to load configuration")?;
    }
    config
        .validate()
        .context("configuration validation failed")?;
    tracing::info!("configuration loaded");
    let refresh = config
        .model
        .as_ref()
        .and_then(|m| m.catalog_refresh)
        .unwrap_or(true);
    if refresh && std::env::var_os("OKEANOS_OFFLINE").is_none() {
        okeanos::model::catalog::spawn_background_refresh();
    }
    okeanos::tool::workspace::set_protect_secrets(
        config
            .permissions
            .as_ref()
            .and_then(|p| p.protect_secrets)
            .unwrap_or(true),
    );

    let mut write_roots = vec![
        cli.root
            .clone()
            .unwrap_or_else(|| std::path::PathBuf::from(".")),
    ];
    write_roots.extend(
        config
            .permissions
            .as_ref()
            .and_then(|p| p.extra_write_roots.as_ref())
            .into_iter()
            .flatten()
            .map(|r| std::path::PathBuf::from(shellexpand::tilde(r).as_ref())),
    );
    let perms = config.permissions.as_ref();
    {
        let project_dir = okeanos::config::trust::project_dir(cli.root.as_deref());
        let trusted = !config.project_trust.is_untrusted();
        let (rules, dropped) = okeanos::harness::exec_policy::load_all(&project_dir, trusted)
            .map_err(anyhow::Error::msg)
            .context("invalid exec-policy file")?;
        if dropped > 0 {
            eprintln!(
                "warning: ignored {dropped} `allow` rule(s) in the untrusted project's {} (run `okeanos trust` to enable)",
                okeanos::harness::exec_policy::FILE_NAME
            );
        }
        okeanos::harness::exec_policy::set_loaded(rules);
    }
    okeanos::tool::run_env::set_allowlist(perms.and_then(|p| p.env_allowlist.clone()));
    okeanos::tool::sandbox::set_policy(okeanos::tool::sandbox::SandboxPolicy {
        mode: perms
            .and_then(|p| p.sandbox.as_deref())
            .and_then(|m| m.parse().ok())
            .unwrap_or_default(),
        network: perms.and_then(|p| p.sandbox_network).unwrap_or(false),
        roots: write_roots.clone(),
    });
    okeanos::tool::workspace::set_roots(write_roots);
    {
        let model_config = config.clone();
        okeanos::harness::subagent::set_model_resolver(Arc::new(move |name| {
            build_named_model(&model_config, name)
        }));
    }

    match cli.mode() {
        Mode::Interactive => run_interactive(cli, &config, prompt).await,
        Mode::Headless => run_headless(cli, &config, prompt).await,
        Mode::Rpc => run_rpc(cli, &config, prompt).await,
        Mode::RpcWs => run_rpc_ws(cli, &config, prompt).await,
        Mode::Acp => run_acp(cli, &config).await,
    }
}
