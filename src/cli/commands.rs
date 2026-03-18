use std::path::Path;

use crate::lua::api::{load_agents_from_files, load_tool_defs_from_files};
use crate::lua::sandbox::create_crew_lua;
use crate::utils::error::{IronCrewError, Result};

use super::project::{load_project, setup_crew_runtime};

pub async fn cmd_run(path: &Path) -> Result<()> {
    let loader = load_project(path)?;
    let (lua, _runtime) = setup_crew_runtime(&loader)?;

    // Execute entrypoint
    let entrypoint = loader
        .entrypoint()
        .ok_or_else(|| IronCrewError::Validation("No entrypoint found".into()))?;
    let script = std::fs::read_to_string(entrypoint)?;

    tracing::info!("Running {}", entrypoint.display());

    lua.load(&script)
        .exec_async()
        .await
        .map_err(IronCrewError::Lua)?;

    Ok(())
}

pub fn cmd_validate(path: &Path) -> Result<()> {
    let loader = load_project(path)?;
    let lua = create_crew_lua().map_err(IronCrewError::Lua)?;

    println!("Validating project: {}", loader.project_dir().display());
    println!();

    // 1. Validate agent files
    let agents = load_agents_from_files(&lua, loader.agent_files()).unwrap_or_default();
    println!("Agents ({}):", agents.len());
    for agent in &agents {
        let mut details = vec![];
        if !agent.capabilities.is_empty() {
            details.push(format!("capabilities: [{}]", agent.capabilities.join(", ")));
        }
        if !agent.tools.is_empty() {
            details.push(format!("tools: [{}]", agent.tools.join(", ")));
        }
        if let Some(ref model) = agent.model {
            details.push(format!("model: {}", model));
        }
        if agent.response_format.is_some() {
            details.push("response_format: set".into());
        }
        let detail_str = if details.is_empty() {
            String::new()
        } else {
            format!(" ({})", details.join(", "))
        };
        println!("  \u{2713} {}{}", agent.name, detail_str);
    }
    if agents.is_empty() {
        println!("  (none -- agents will be defined inline in crew.lua)");
    }
    println!();

    // 2. Validate tool files
    let tool_defs = load_tool_defs_from_files(&lua, loader.tool_files()).unwrap_or_default();
    let known_tools: Vec<String> = vec![
        "file_read",
        "file_write",
        "web_scrape",
        "shell",
        "http_request",
        "hash",
        "template_render",
    ]
    .into_iter()
    .map(String::from)
    .chain(tool_defs.iter().map(|t| t.name.clone()))
    .collect();

    println!("Tools ({} built-in + {} custom):", 7, tool_defs.len());
    println!("  Built-in: file_read, file_write, web_scrape, shell, http_request, hash, template_render");
    for tool in &tool_defs {
        println!(
            "  \u{2713} {} (custom, from {})",
            tool.name,
            tool.source_path.display()
        );
    }
    println!();

    // 3. Validate entrypoint syntax
    if let Some(entrypoint) = loader.entrypoint() {
        let script = std::fs::read_to_string(entrypoint)?;
        lua.load(&script).into_function().map_err(|e| {
            IronCrewError::Validation(format!("Syntax error in {}: {}", entrypoint.display(), e))
        })?;
        println!("Entrypoint: \u{2713} {} (syntax valid)", entrypoint.display());
    }
    println!();

    // 4. Reference integrity: agent tool references
    let mut issues = 0;
    for agent in &agents {
        for tool_name in &agent.tools {
            if !known_tools.contains(tool_name) {
                println!(
                    "  \u{2717} Agent '{}' references unknown tool '{}'",
                    agent.name, tool_name
                );
                issues += 1;
            }
        }
    }

    if issues == 0 {
        println!("Reference integrity: \u{2713} all references valid");
    }
    println!();

    // 5. Summary
    if issues > 0 {
        println!("Validation FAILED with {} issue(s).", issues);
        Err(IronCrewError::Validation(format!(
            "{} validation issue(s) found",
            issues
        )))
    } else {
        println!("Validation PASSED.");
        println!();
        println!("Note: Task dependencies and execution order are validated at runtime");
        println!("      when crew.lua is executed (tasks are defined programmatically).");
        Ok(())
    }
}

pub fn cmd_init(name: &str) -> Result<()> {
    let project_dir = Path::new(name);

    if project_dir.exists() {
        return Err(IronCrewError::Validation(format!(
            "Directory '{}' already exists",
            name
        )));
    }

    // Create directory structure
    std::fs::create_dir_all(project_dir.join("agents"))?;
    std::fs::create_dir_all(project_dir.join("tools"))?;

    // Write .env template
    std::fs::write(
        project_dir.join(".env"),
        "# IronCrew Environment Configuration\n\
         OPENAI_API_KEY=your-api-key-here\n\
         OPENAI_BASE_URL=https://api.openai.com/v1\n\
         OPENAI_MODEL=gpt-4o-mini\n\
         IRONCREW_LOG=info\n",
    )?;

    // Write .gitignore
    std::fs::write(
        project_dir.join(".gitignore"),
        "/output\n\
         .env\n\
         .DS_Store\n\
         .ironcrew/\n",
    )?;

    // Write sample agent
    std::fs::write(
        project_dir.join("agents/assistant.lua"),
        r#"return {
    name = "assistant",
    goal = "Help with tasks by providing clear, accurate responses",
    capabilities = {"general", "analysis", "writing"},
    temperature = 0.7,
}
"#,
    )?;

    // Write crew.lua entrypoint
    std::fs::write(
        project_dir.join("crew.lua"),
        format!(
            r#"--[[
    {name} - IronCrew Project

    Run with: ironcrew run .
    Validate with: ironcrew validate .
]]

local crew = Crew.new({{
    goal = "Your crew goal here",
    provider = "openai",
    model = env("OPENAI_MODEL") or "gpt-4o-mini",
    base_url = env("OPENAI_BASE_URL"),
}})

-- Add tasks
crew:add_task({{
    name = "hello",
    description = "Say hello and introduce yourself briefly",
    expected_output = "A friendly greeting",
}})

-- Run the crew
local results = crew:run()

-- Display results
for _, result in ipairs(results) do
    if result.success then
        print("=== " .. result.task .. " (" .. result.duration_ms .. "ms) ===")
        print(result.output)
    else
        print("FAILED: " .. result.task .. " - " .. result.output)
    end
    print()
end
"#,
            name = name
        ),
    )?;

    println!("Created new IronCrew project: {}", name);
    println!();
    println!("  {}/", name);
    println!("  \u{251c}\u{2500}\u{2500} .env              # API keys and config");
    println!("  \u{251c}\u{2500}\u{2500} .gitignore");
    println!("  \u{251c}\u{2500}\u{2500} agents/");
    println!("  \u{2502}   \u{2514}\u{2500}\u{2500} assistant.lua # Sample agent");
    println!("  \u{251c}\u{2500}\u{2500} tools/            # Custom tools (empty)");
    println!("  \u{2514}\u{2500}\u{2500} crew.lua          # Entrypoint");
    println!();
    println!("Next steps:");
    println!("  1. cd {}", name);
    println!("  2. Edit .env with your API key");
    println!("  3. ironcrew run .");

    Ok(())
}

pub fn cmd_nodes() -> Result<()> {
    // Create a temporary registry to get all built-in tools
    let mut registry = crate::tools::registry::ToolRegistry::new();

    // Register all built-in tools
    registry.register(Box::new(crate::tools::file_read::FileReadTool::new(None)));
    registry.register(Box::new(crate::tools::file_write::FileWriteTool::new(
        None, None,
    )));
    registry.register(Box::new(crate::tools::web_scrape::WebScrapeTool::new(
        None,
    )));
    registry.register(Box::new(crate::tools::shell::ShellTool::new()));
    registry.register(Box::new(
        crate::tools::http_request::HttpRequestTool::new(),
    ));
    registry.register(Box::new(crate::tools::hash::HashTool::new()));
    registry.register(Box::new(
        crate::tools::template_render::TemplateRenderTool::new(),
    ));

    println!("Built-in tools ({}):", registry.list().len());
    println!();

    // Get tools sorted by name
    let mut tools: Vec<(String, String)> = Vec::new();
    for name in registry.list() {
        if let Some(tool) = registry.get(&name) {
            tools.push((name, tool.description().to_string()));
        }
    }
    tools.sort_by(|a, b| a.0.cmp(&b.0));

    // Find the longest name for alignment
    let max_name_len = tools.iter().map(|(n, _)| n.len()).max().unwrap_or(0);

    for (name, description) in &tools {
        println!("  {:<width$}  {}", name, description, width = max_name_len);
    }

    println!();
    println!("Custom tools can be defined in tools/*.lua files in your project.");

    Ok(())
}

pub fn cmd_list(path: &Path) -> Result<()> {
    let loader = load_project(path)?;
    let lua = create_crew_lua().map_err(IronCrewError::Lua)?;

    println!("Project: {}", loader.project_dir().display());
    println!();

    // List agents
    let agents = load_agents_from_files(&lua, loader.agent_files()).unwrap_or_default();
    println!("Agents ({}):", agents.len());
    for agent in &agents {
        println!("  {} -- {}", agent.name, agent.goal);
        if !agent.capabilities.is_empty() {
            println!("    capabilities: [{}]", agent.capabilities.join(", "));
        }
        if !agent.tools.is_empty() {
            println!("    tools: [{}]", agent.tools.join(", "));
        }
        if let Some(ref model) = agent.model {
            println!("    model: {}", model);
        }
        if let Some(temp) = agent.temperature {
            println!("    temperature: {}", temp);
        }
        if agent.response_format.is_some() {
            println!("    response_format: configured");
        }
    }
    println!();

    // List tool files
    let tool_defs = load_tool_defs_from_files(&lua, loader.tool_files()).unwrap_or_default();
    println!("Custom tools ({}):", tool_defs.len());
    for tool in &tool_defs {
        println!(
            "  {} -- from {}",
            tool.name,
            tool.source_path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
        );
    }
    if tool_defs.is_empty() {
        println!("  (none)");
    }
    println!();

    println!("Built-in tools (7): file_read, file_write, web_scrape, shell, http_request, hash, template_render");
    println!();

    // Entrypoint
    if let Some(ep) = loader.entrypoint() {
        println!("Entrypoint: {}", ep.display());
    }

    Ok(())
}
