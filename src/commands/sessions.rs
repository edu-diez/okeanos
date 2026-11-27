use crate::prelude::*;

/// List resumable sessions.
pub(crate) async fn cmd_sessions(cli: &Cli) -> anyhow::Result<()> {
    let config = Config::load_raw(cli.root.as_deref()).context("failed to load configuration")?;

    let project_root = cli
        .root
        .as_deref()
        .unwrap_or_else(|| std::path::Path::new("."));
    let project_root =
        std::fs::canonicalize(project_root).context("failed to resolve project root")?;

    let session_manager = SessionManager::new_with_config(&project_root, config.rotation.clone())
        .context("failed to create session manager")?;
    let sessions = session_manager
        .list_sessions()
        .context("failed to list sessions")?;

    if sessions.is_empty() {
        println!(
            "No sessions found in {}",
            project_root.join(".okeanos/sessions").display()
        );
        return Ok(());
    }

    println!(
        "{:<40} {:<20} {:<40} {:>8}",
        "SESSION ID", "STARTED", "PROJECT", "EVENTS"
    );
    println!("{}", "-".repeat(110));

    for session in sessions {
        let started = chrono::DateTime::from_timestamp_millis(session.started_at as i64)
            .map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string())
            .unwrap_or_else(|| "unknown".to_string());
        let project = session
            .project_root
            .split('/')
            .next_back()
            .unwrap_or(&session.project_root);

        println!(
            "{:<40} {:<20} {:<40} {:>8}",
            session.id, started, project, session.event_count
        );
    }

    Ok(())
}
