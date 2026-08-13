use std::path::Path;

use crate::llm::DEFAULT_OPENAI_MODEL;
use crate::utils::error::{IronCrewError, Result};

pub fn cmd_init(name: &str) -> Result<()> {
    let project_dir = Path::new(name);

    if project_dir.exists() {
        return Err(IronCrewError::Validation(format!(
            "Directory '{}' already exists",
            name
        )));
    }

    std::fs::create_dir_all(project_dir.join("agents"))?;
    std::fs::create_dir_all(project_dir.join("tools"))?;

    std::fs::write(
        project_dir.join(".env"),
        format!(
            "# IronCrew Environment Configuration\n\
             OPENAI_API_KEY=your-api-key-here\n\
             OPENAI_BASE_URL=https://api.openai.com/v1\n\
             OPENAI_MODEL={DEFAULT_OPENAI_MODEL}\n\
             IRONCREW_LOG=info\n"
        ),
    )?;

    std::fs::write(
        project_dir.join(".gitignore"),
        "/output\n\
         .env\n\
         .DS_Store\n\
         .ironcrew/\n",
    )?;

    std::fs::write(
        project_dir.join("agents/assistant.lua"),
        r#"return {
    name = "assistant",
    goal = "Help with tasks by providing clear, accurate responses",
    capabilities = {"general", "analysis", "writing"},
}
"#,
    )?;

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
    model = env("OPENAI_MODEL") or "{default_model}",
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
            name = name,
            default_model = DEFAULT_OPENAI_MODEL,
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
