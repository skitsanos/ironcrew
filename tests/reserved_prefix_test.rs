//! Integration test: custom Lua tool definitions must not claim the reserved
//! `agent__` namespace. The rejection must happen in `load_tool_defs_from_files`
//! — before any agent is wired up.

use ironcrew::lua::api::load_tool_defs_from_files;
use ironcrew::lua::loader::ProjectLoader;

#[test]
fn reserved_agent_prefix_on_custom_tool_rejected() {
    // Write a minimal tool file that claims the reserved prefix.
    let dir = tempfile::tempdir().unwrap();
    let tools_dir = dir.path().join("tools");
    std::fs::create_dir_all(&tools_dir).unwrap();
    std::fs::write(
        tools_dir.join("bad.lua"),
        r#"return {
            name        = "agent__hijack",
            description = "tries to impersonate an agent tool",
            parameters  = {},
            execute     = function(_) return "nope" end,
        }"#,
    )
    .unwrap();
    // crew.lua is required by ProjectLoader::from_directory
    std::fs::write(dir.path().join("crew.lua"), "-- empty").unwrap();

    let loader = ProjectLoader::from_directory(dir.path()).expect("loader");
    match load_tool_defs_from_files(loader.tool_files()) {
        Ok(_) => panic!("expected an error but tool loaded successfully"),
        Err(e) => {
            let msg = e.to_string();
            assert!(
                msg.contains("agent__") && msg.to_lowercase().contains("reserved"),
                "expected 'reserved agent__' error, got: {msg}"
            );
        }
    }
}

#[test]
fn non_reserved_prefix_tool_loads_fine() {
    let dir = tempfile::tempdir().unwrap();
    let tools_dir = dir.path().join("tools");
    std::fs::create_dir_all(&tools_dir).unwrap();
    std::fs::write(
        tools_dir.join("ok.lua"),
        r#"return {
            name        = "my_tool",
            description = "a perfectly normal tool",
            parameters  = {},
            execute     = function(_) return "ok" end,
        }"#,
    )
    .unwrap();
    std::fs::write(dir.path().join("crew.lua"), "-- empty").unwrap();

    let loader = ProjectLoader::from_directory(dir.path()).expect("loader");
    let defs = load_tool_defs_from_files(loader.tool_files()).expect("should load fine");

    assert_eq!(defs.len(), 1);
    assert_eq!(defs[0].name, "my_tool");
}
