use std::path::Path;

use crate::engine::run_history::RunStatus;
use crate::engine::store::create_store;
use crate::utils::error::Result;

pub fn cmd_runs(project: &Path, status_filter: Option<&str>) -> Result<()> {
    let ironcrew_dir = project.join(".ironcrew");
    let store = create_store(ironcrew_dir)?;
    let runs = store.list_runs(status_filter)?;

    if runs.is_empty() {
        println!("No runs found.");
        return Ok(());
    }

    let header_started = "STARTED";
    println!(
        "{:<38} {:<16} {:<10} {:<10} {}",
        "RUN ID", "STATUS", "TASKS", "DURATION", header_started
    );
    println!("{}", "-".repeat(90));

    for run in &runs {
        let status_display = match run.status {
            RunStatus::Success => "success",
            RunStatus::PartialFailure => "partial",
            RunStatus::Failed => "failed",
        };
        println!(
            "{:<38} {:<16} {:<10} {:<10} {}",
            run.run_id,
            status_display,
            format!(
                "{}/{}",
                run.task_results.iter().filter(|r| r.success).count(),
                run.task_count
            ),
            format!("{}ms", run.duration_ms),
            if run.started_at.len() >= 19 {
                &run.started_at[..19]
            } else {
                &run.started_at
            },
        );
    }

    println!("\n{} run(s) total.", runs.len());
    Ok(())
}

pub fn cmd_inspect(project: &Path, run_id: &str) -> Result<()> {
    let ironcrew_dir = project.join(".ironcrew");
    let store = create_store(ironcrew_dir)?;
    let record = store.get_run(run_id)?;

    println!("Run: {}", record.run_id);
    println!("Flow: {}", record.flow_name);
    println!("Status: {}", record.status);
    println!("Started: {}", record.started_at);
    println!("Finished: {}", record.finished_at);
    println!("Duration: {}ms", record.duration_ms);
    println!(
        "Tokens: {} total ({} cached)",
        record.total_tokens, record.cached_tokens
    );
    println!("Agents: {}", record.agent_count);
    println!(
        "Tasks: {}/{} succeeded",
        record.task_results.iter().filter(|r| r.success).count(),
        record.task_count
    );
    println!();

    for result in &record.task_results {
        let status = if result.success { "OK" } else { "FAIL" };
        let agent = if result.agent.is_empty() {
            "(none)"
        } else {
            &result.agent
        };
        println!(
            "[{}] {} (by {}, {}ms)",
            status, result.task, agent, result.duration_ms
        );
        // Truncate long output
        let output = if result.output.len() > 200 {
            format!("{}...", &result.output[..200])
        } else {
            result.output.clone()
        };
        println!("  {}", output);
        println!();
    }

    Ok(())
}

pub fn cmd_clean(project: &Path, keep: usize, all: bool) -> Result<()> {
    let ironcrew_dir = project.join(".ironcrew");

    if !ironcrew_dir.exists() {
        println!("No run history found.");
        return Ok(());
    }

    let store = create_store(ironcrew_dir)?;
    let runs = store.list_runs(None)?;

    if runs.is_empty() {
        println!("No runs to clean.");
        return Ok(());
    }

    if all {
        // Delete everything
        let count = runs.len();
        for run in &runs {
            store.delete_run(&run.run_id)?;
        }
        println!("Deleted all {} run(s).", count);

        // Also clean up memory file if it exists
        let memory_file = project.join(".ironcrew").join("memory.json");
        if memory_file.exists() {
            std::fs::remove_file(&memory_file)?;
            println!("Deleted memory store.");
        }
    } else {
        // Keep the last N, delete the rest
        // runs are already sorted newest-first from store.list_runs()
        if runs.len() <= keep {
            println!(
                "Only {} run(s) found, nothing to clean (keeping {}).",
                runs.len(),
                keep
            );
            return Ok(());
        }

        let to_delete = &runs[keep..];
        let count = to_delete.len();
        for run in to_delete {
            store.delete_run(&run.run_id)?;
        }
        println!("Deleted {} old run(s), kept {} most recent.", count, keep);
    }

    Ok(())
}
