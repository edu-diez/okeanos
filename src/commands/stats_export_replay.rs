use crate::prelude::*;

/// Load session entries from a session id or a `.jsonl` file path.
pub(crate) fn load_entries_for(
    cli: &Cli,
    session: &str,
) -> anyhow::Result<(String, Vec<okeanos::session::header::SessionEntry>)> {
    let path = std::path::Path::new(session);
    if path.is_file() {
        let text = std::fs::read_to_string(path).context("failed to read session file")?;
        let entries = text
            .lines()
            .skip(1)
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        return Ok((session.to_string(), entries));
    }
    let config = Config::load_raw(cli.root.as_deref()).context("failed to load configuration")?;
    let root = std::fs::canonicalize(cli.root.as_deref().unwrap_or(std::path::Path::new(".")))
        .context("failed to resolve project root")?;
    let sm = SessionManager::new_with_config(&root, config.rotation.clone())
        .context("failed to create session manager")?;
    let (_, entries, warnings) = sm
        .load_session_entries(session)
        .with_context(|| format!("failed to load session {session}"))?;
    for w in warnings {
        eprintln!("warning: {w}");
    }
    Ok((session.to_string(), entries))
}

pub(crate) fn cmd_stats(cli: &Cli, session: &str, json: bool) -> anyhow::Result<()> {
    let (_, entries) = load_entries_for(cli, session)?;
    let stats = okeanos::observability::stats::SessionStats::from_entries(&entries);
    if json {
        println!("{}", serde_json::to_string_pretty(&stats)?);
    } else {
        println!("{}", stats.render());
    }
    Ok(())
}

pub(crate) fn cmd_export(cli: &Cli, session: &str, format: &str) -> anyhow::Result<()> {
    let (id, entries) = load_entries_for(cli, session)?;
    match format {
        "md" | "markdown" => println!(
            "{}",
            okeanos::observability::export::render_markdown(&id, &entries)
        ),
        "json" => println!("{}", serde_json::to_string_pretty(&entries)?),
        other => anyhow::bail!("unknown export format '{other}' (use md or json)"),
    }
    Ok(())
}

pub(crate) fn cmd_replay(cli: &Cli, session: &str) -> anyhow::Result<()> {
    let (_, entries) = load_entries_for(cli, session)?;
    println!(
        "{}",
        okeanos::observability::export::render_timeline(&entries)
    );
    Ok(())
}
