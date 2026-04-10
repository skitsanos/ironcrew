use std::path::Path;

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
