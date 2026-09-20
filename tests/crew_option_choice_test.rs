//! Regression: documented enum options on `Crew.new` are validated at
//! construction (not forwarded verbatim for the provider to reject later).

use std::fs;

use ironcrew::cli::project::{load_project, setup_crew_runtime};

#[tokio::test]
async fn reasoning_effort_typo_fails_at_crew_construction() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("crew.lua"),
        r#"
local ok, err = pcall(Crew.new, {
    goal = "enum validation contract",
    provider = "openai-responses",
    api_key = "test-key",
    model = "gpt-5.6-luna",
    reasoning_effort = "lwo",
})
assert(not ok, "a misspelled reasoning_effort must be rejected at Crew.new")
local message = tostring(err)
assert(message:find("reasoning_effort", 1, true), message)
assert(message:find("'lwo'", 1, true), message)
assert(message:find("low, medium, high", 1, true), message)

-- The documented values still construct.
local crew = Crew.new({
    goal = "enum validation contract",
    provider = "openai-responses",
    api_key = "test-key",
    model = "gpt-5.6-luna",
    reasoning_effort = "low",
    reasoning_summary = "concise",
})
assert(crew ~= nil)
"#,
    )
    .unwrap();

    let loader = load_project(dir.path()).unwrap();
    let (lua, _runtime) = setup_crew_runtime(&loader).unwrap();
    let script = fs::read_to_string(dir.path().join("crew.lua")).unwrap();
    lua.load(&script)
        .exec_async()
        .await
        .expect("crew.lua assertions must hold");
}
