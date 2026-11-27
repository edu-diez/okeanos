use crate::prelude::*;

/// `okeanos models update|show`.
pub(crate) async fn cmd_models(
    cli: &okeanos::cli::Cli,
    action: &okeanos::cli::ModelsAction,
) -> anyhow::Result<()> {
    use okeanos::cli::ModelsAction;
    use okeanos::model::catalog;
    match action {
        ModelsAction::Update { out } => {
            let entries = catalog::fetch()
                .await
                .context("failed to fetch models.dev")?;
            let path = match out {
                Some(p) => p.clone(),
                None => catalog::cache_path().context("no cache directory available")?,
            };
            catalog::write_entries(&path, &entries)?;
            println!("Wrote {} models to {}", entries.len(), path.display());
        }
        ModelsAction::Show { name } => {
            let c = okeanos::model::classify_model(name);
            println!(
                "tier: {:?} (confidence {:.2}) - {}",
                c.tier, c.confidence, c.reason
            );
            match catalog::lookup(name) {
                Some(e) => println!(
                    "catalog: {} context={} max_output={} tools={} vision={} reasoning={} price=${}/{} per M",
                    e.id,
                    e.context,
                    e.max_output,
                    e.tool_calling,
                    e.vision,
                    e.reasoning,
                    e.input_per_million,
                    e.output_per_million
                ),
                None => println!("catalog: not found (using name heuristics)"),
            }
            print_effective(cli, name, &c);
        }
    }
    Ok(())
}

/// Print the values the harness would use for `name`, and where each came from.
fn print_effective(
    cli: &okeanos::cli::Cli,
    name: &str,
    classification: &okeanos::model::ModelClassification,
) {
    use okeanos::model::ModelTier;
    use okeanos::tool::builtin::EditFormat;
    let config = Config::load_raw(cli.root.as_deref()).unwrap_or_default();
    let over = config.model_override(name);
    let src = |key: &str| format!("[models.\"{key}\"]");
    let pinned = config.model.as_ref().and_then(|m| m.tier.as_deref());

    let learned = okeanos::model::profile::load(name);
    let (tier, tier_src) = match (
        over.and_then(|(k, o)| o.tier.as_deref().map(|t| (k, t))),
        pinned,
    ) {
        (Some((k, t)), _) => (t.parse().unwrap_or(classification.tier), src(k)),
        (None, Some(t)) => (
            t.parse().unwrap_or(classification.tier),
            "config model.tier".into(),
        ),
        (None, None) => match learned {
            Some(l) if l.rank() < classification.tier.rank() => {
                (l, "learned profile (lowered at runtime)".to_string())
            }
            _ => (classification.tier, "classifier".to_string()),
        },
    };
    let tier: ModelTier = tier;

    println!("effective:");
    println!("  tier:             {tier} ({tier_src})");
    let (fmt, fmt_src) = match over.and_then(|(k, o)| o.edit_format.as_deref().map(|f| (k, f))) {
        Some((k, f)) => (f.to_string(), src(k)),
        None => (
            format!("{:?}", EditFormat::default_for_tier(tier)).to_lowercase(),
            "tier default".to_string(),
        ),
    };
    println!("  edit_format:      {fmt} ({fmt_src})");
    match over.and_then(|(k, o)| o.temperature.map(|t| (k, t))) {
        Some((k, t)) => println!("  temperature:      {t} ({})", src(k)),
        None => println!("  temperature:      tier default"),
    }
    match over.and_then(|(k, o)| o.tool_output_cap.map(|t| (k, t))) {
        Some((k, t)) => println!("  tool_output_cap:  {t} ({})", src(k)),
        None => println!("  tool_output_cap:  tier default"),
    }
    match over.and_then(|(k, o)| o.native_tools.map(|t| (k, t))) {
        Some((k, t)) => println!("  native_tools:     {t} ({})", src(k)),
        None => println!("  native_tools:     catalog/default"),
    }
    match over.and_then(|(k, o)| o.max_parallel_tools.map(|t| (k, t))) {
        Some((k, t)) => println!("  max_parallel:     {t} ({})", src(k)),
        None => println!("  max_parallel:     unlimited"),
    }
    match over.and_then(|(k, o)| o.verify_tiers.as_ref().map(|t| (k, t))) {
        Some((k, t)) => println!("  verify_tiers:     {} ({})", t.join(","), src(k)),
        None => println!("  verify_tiers:     [verify] section / small"),
    }
}
