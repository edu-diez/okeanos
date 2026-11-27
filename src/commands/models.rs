use crate::prelude::*;

/// `okeanos trust [--revoke]`.
pub(crate) async fn cmd_models(action: &okeanos::cli::ModelsAction) -> anyhow::Result<()> {
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
        }
    }
    Ok(())
}
