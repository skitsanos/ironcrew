use ironcrew::engine::agent::{Agent, AgentSelector};
use ironcrew::engine::task::Task;

#[test]
fn test_select_agent_by_capability_match() {
    let agents = vec![
        Agent {
            name: "writer".into(),
            goal: "Write content".into(),
            capabilities: vec!["writing".into(), "editing".into()],
            ..Default::default()
        },
        Agent {
            name: "researcher".into(),
            goal: "Research topics".into(),
            capabilities: vec!["research".into(), "analysis".into()],
            ..Default::default()
        },
    ];

    let task = Task {
        name: "research_task".into(),
        description: "Research the latest AI trends and analysis".into(),
        ..Default::default()
    };

    let selected = AgentSelector::select(&agents, &task);
    assert_eq!(selected.name, "researcher");
}

#[test]
fn test_select_agent_by_tool_match() {
    let agents = vec![
        Agent {
            name: "writer".into(),
            goal: "Write files".into(),
            capabilities: vec!["writing".into()],
            tools: vec!["file_write".into()],
            ..Default::default()
        },
        Agent {
            name: "scraper".into(),
            goal: "Scrape the web".into(),
            capabilities: vec!["scraping".into()],
            tools: vec!["web_scrape".into()],
            ..Default::default()
        },
    ];

    let task = Task {
        name: "scrape_task".into(),
        description: "Scrape data from websites".into(),
        ..Default::default()
    };

    let selected = AgentSelector::select(&agents, &task);
    assert_eq!(selected.name, "scraper");
}

#[test]
fn test_select_fallback_to_first_agent() {
    let agents = vec![
        Agent {
            name: "alpha".into(),
            goal: "Do alpha things".into(),
            ..Default::default()
        },
        Agent {
            name: "beta".into(),
            goal: "Do beta things".into(),
            ..Default::default()
        },
    ];

    let task = Task {
        name: "unrelated".into(),
        description: "Something completely unrelated to any agent".into(),
        ..Default::default()
    };

    let selected = AgentSelector::select(&agents, &task);
    assert_eq!(selected.name, "alpha");
}
