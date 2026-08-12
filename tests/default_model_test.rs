use std::fs;

use ironcrew::cli::commands::cmd_init;
use ironcrew::cli::graph_extract::extract_graph_data;

const EXPECTED_DEFAULT_MODEL: &str = "gpt-5.6-luna";

#[test]
fn graph_capture_uses_the_current_default_model() {
    let workspace = tempfile::tempdir().expect("create temporary graph workspace");
    fs::write(
        workspace.path().join("crew.lua"),
        r#"local crew = Crew.new({ goal = "default model contract" })
return crew
"#,
    )
    .expect("write crew fixture");

    let graph = extract_graph_data(workspace.path()).expect("extract graph data");
    assert_eq!(graph.model, EXPECTED_DEFAULT_MODEL);
}

#[test]
fn init_templates_use_the_current_default_model() {
    let workspace = tempfile::tempdir().expect("create temporary init workspace");
    let project = workspace.path().join("demo");

    cmd_init(project.to_str().expect("UTF-8 temporary path")).expect("initialize project");

    let env_template = fs::read_to_string(project.join(".env")).expect("read .env template");
    let crew_template = fs::read_to_string(project.join("crew.lua")).expect("read crew template");
    let agent_template =
        fs::read_to_string(project.join("agents/assistant.lua")).expect("read agent template");
    assert!(env_template.contains(&format!("OPENAI_MODEL={EXPECTED_DEFAULT_MODEL}")));
    assert!(crew_template.contains(&format!("or \"{EXPECTED_DEFAULT_MODEL}\"")));
    assert!(!agent_template.contains("temperature"));
}
