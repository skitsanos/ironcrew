use std::path::Path;

/// Regression test: `postgres.*` calls embedded in crew.lua (even inline
/// inside the `Crew.new()` argument table) must not crash graph extraction.
/// The capture-mode VM stubs `http`, `json_parse`, `print`, `error`, etc. for
/// exactly this reason, but previously lacked a `postgres` stub, so a script
/// that called `postgres.execute/query/query_one` before or while building
/// its `Crew.new()` table hit a nil-index and lost the capture (both the
/// direct pass and the `Crew.new(`-only fallback pass re-hit the same crash).
#[test]
fn graph_extraction_survives_postgres_calls_in_crew_new_args() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("crew.lua"),
        r#"
Crew.new({
    goal = "processed " .. tostring(postgres.execute("noop", {})) .. " rows",
    provider = "openai",
    api_key = "test",
})
"#,
    )
    .unwrap();

    let data = ironcrew::cli::graph_extract::extract_graph_data(dir.path()).unwrap();
    assert_eq!(
        data.goal, "processed 0 rows",
        "postgres.* stub should let Crew.new()'s goal expression evaluate instead of \
         silently losing the capture to a nil-index crash"
    );
}

#[test]
fn extract_research_crew_data() {
    let data =
        ironcrew::cli::graph_extract::extract_graph_data(Path::new("examples/research-crew"))
            .unwrap();

    // Crew metadata
    assert_eq!(data.name, "research-crew");
    assert_eq!(data.agents.len(), 2);
    assert_eq!(data.tasks.len(), 2);
    assert_eq!(data.tools.len(), 1);

    // Agents
    let researcher = data.agents.iter().find(|a| a.name == "researcher").unwrap();
    assert_eq!(researcher.source, "auto_discovered");
    assert!(researcher.capabilities.contains(&"research".to_string()));

    let writer = data.agents.iter().find(|a| a.name == "writer").unwrap();
    assert!(writer.tools.contains(&"summarize".to_string()));

    // Tasks
    let research = data.tasks.iter().find(|t| t.id == "research").unwrap();
    assert!(research.depends_on.is_empty());
    assert_eq!(research.assignment_source, "auto");

    let write_summary = data.tasks.iter().find(|t| t.id == "write_summary").unwrap();
    assert_eq!(write_summary.depends_on, vec!["research"]);
    assert_eq!(write_summary.agent.as_deref(), Some("writer"));
    assert_eq!(write_summary.assignment_source, "explicit");

    // Tool
    assert_eq!(data.tools[0].name, "summarize");
}

#[test]
fn generate_html_produces_valid_file() {
    let data =
        ironcrew::cli::graph_extract::extract_graph_data(Path::new("examples/research-crew"))
            .unwrap();

    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("test-graph.html");

    ironcrew::cli::graph_bundle::generate_html(&data, &output).unwrap();

    let html = std::fs::read_to_string(&output).unwrap();

    // Contains crew data
    assert!(html.contains("research-crew"));
    assert!(html.contains("researcher"));
    assert!(html.contains("write_summary"));

    // Contains embedded assets
    assert!(html.contains("@antv/x6"));
    assert!(html.contains("IBM Plex Sans"));
    assert!(html.contains("ironcrew-task"));
    assert!(html.contains("__ICON_DATA_URIS"));
    assert!(html.contains("data:image/svg+xml"));
}

#[test]
fn hitl_examples_capture_their_human_control_contracts() {
    let ask =
        ironcrew::cli::graph_extract::extract_graph_data(Path::new("examples/ask-human")).unwrap();
    assert_eq!(ask.human_inputs.len(), 2);
    assert_eq!(
        ask.human_inputs[0].prompt,
        "What should the announcement be about?"
    );
    assert_eq!(ask.human_inputs[1].prompt, "Publish this draft?");
    assert_eq!(
        ask.human_inputs[1].choices,
        vec!["publish".to_string(), "hold".to_string()]
    );

    let approval =
        ironcrew::cli::graph_extract::extract_graph_data(Path::new("examples/human-approval"))
            .unwrap();
    assert_eq!(approval.require_approval, vec!["file_write"]);
    let agent = approval
        .agents
        .iter()
        .find(|agent| agent.name == "release_manager")
        .unwrap();
    assert_eq!(agent.tools, vec!["ask_human", "file_write"]);
}
