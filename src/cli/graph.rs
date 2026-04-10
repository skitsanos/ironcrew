use std::path::{Path, PathBuf};

use crate::cli::graph_bundle::generate_html;
use crate::cli::graph_extract::extract_graph_data;
use crate::utils::error::Result;

/// Generate a DAG visualization HTML file for a crew project.
pub fn cmd_graph(path: &Path, output: Option<&Path>) -> Result<()> {
    println!("Extracting graph data from {}...", path.display());

    let data = extract_graph_data(path)?;

    let output_path = output
        .map(PathBuf::from)
        .unwrap_or_else(|| path.join("graph.html"));

    generate_html(&data, &output_path)?;

    println!();
    println!("Crew: {} ({})", data.name, data.goal);
    println!(
        "  {} agent(s), {} task(s), {} tool(s)",
        data.agents.len(),
        data.tasks.len(),
        data.tools.len()
    );

    if data.agents.is_empty() && data.tasks.is_empty() {
        println!();
        println!(
            "  Warning: no agents or tasks were captured. This can happen when"
        );
        println!(
            "  crew.lua depends on runtime data (HTTP fetches, API calls) that"
        );
        println!(
            "  isn't available during static analysis. The graph will show only"
        );
        println!("  the crew and result nodes.");
    }

    println!();
    println!("Graph saved to: {}", output_path.display());
    println!("Open in a browser to view the interactive DAG.");

    Ok(())
}
