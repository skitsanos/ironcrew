//! Regression test: `config.lua` must be able to see the fail-closed
//! `postgres.*` stub during evaluation. `postgres.*` registration used to run
//! *after* config.lua was evaluated inside `setup_crew_runtime_inner`, so a
//! config.lua touching `postgres.*` (e.g. to sanity-check the configuration
//! hint) hit a nil-index instead of the diagnosable stub error that docs
//! promise.

use std::fs;

use ironcrew::cli::project::setup_crew_runtime;
use ironcrew::lua::loader::ProjectLoader;

#[test]
fn config_lua_can_observe_postgres_stub_during_evaluation() {
    // Config.lua is evaluated as part of setup_crew_runtime; make sure no
    // stray app-db URL leaks in from the test environment so the fail-closed
    // "unconfigured" stub (not a live connection attempt) is what
    // config.lua's assertions observe.
    unsafe {
        std::env::remove_var("IRONCREW_APP_DATABASE_URL");
    }

    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("crew.lua"), "return 1").unwrap();
    fs::write(
        dir.path().join("config.lua"),
        r#"
local ok, err = pcall(function() return postgres.query("x") end)
assert(not ok, "expected stub error")
assert(tostring(err):find("IRONCREW_APP_DATABASE_URL", 1, true), tostring(err))
return {}
"#,
    )
    .unwrap();

    let loader = ProjectLoader::from_directory(dir.path()).unwrap();
    setup_crew_runtime(&loader).expect("config.lua should observe the registered postgres.* stub");
}
