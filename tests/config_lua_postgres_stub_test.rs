//! Regression test: `config.lua` is declarative and any `postgres.*` call must
//! fail through the config-evaluation purity boundary before database work.

use std::fs;

use ironcrew::cli::project::setup_crew_runtime;
use ironcrew::lua::loader::ProjectLoader;

#[test]
fn config_lua_rejects_postgres_effects_during_evaluation() {
    // Config.lua is evaluated as part of setup_crew_runtime; make sure no
    // stray app-db URL leaks in from the test environment so the fail-closed
    // "unconfigured" stub (not a live connection attempt) is what
    // config.lua's assertions observe.
    //
    // NOTE: process-env mutation is safe only while this file stays a
    // single-test binary. If more tests are ever added here, they will run in
    // parallel threads sharing this environment — move the env handling to a
    // serialized harness first.
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
assert(tostring(err):find("config.lua evaluation", 1, true), tostring(err))
return {}
"#,
    )
    .unwrap();

    let loader = ProjectLoader::from_directory(dir.path()).unwrap();
    setup_crew_runtime(&loader).expect("config.lua should catch the purity error");
}
