//! Verify that the canonical `IRONCREW_MODE` Lua global is set correctly
//! in both normal run mode and chat mode. Users guard top-level
//! `crew:run()` calls with `if IRONCREW_MODE ~= "chat" then ... end`.

use ironcrew::lua::api::set_ironcrew_mode;
use ironcrew::lua::sandbox::create_crew_lua;

#[test]
fn ironcrew_mode_defaults_to_run_when_set_explicitly() {
    let lua = create_crew_lua().unwrap();
    set_ironcrew_mode(&lua, "run").unwrap();
    let mode: String = lua.load(r#"return IRONCREW_MODE"#).eval().unwrap();
    assert_eq!(mode, "run");
}

#[test]
fn ironcrew_mode_can_be_switched_to_chat() {
    let lua = create_crew_lua().unwrap();
    set_ironcrew_mode(&lua, "chat").unwrap();
    let mode: String = lua.load(r#"return IRONCREW_MODE"#).eval().unwrap();
    assert_eq!(mode, "chat");
}

#[test]
fn ironcrew_mode_guard_pattern_works_for_run() {
    let lua = create_crew_lua().unwrap();
    set_ironcrew_mode(&lua, "run").unwrap();
    let should_run: bool = lua
        .load(r#"return IRONCREW_MODE ~= "chat""#)
        .eval()
        .unwrap();
    assert!(should_run, "in 'run' mode the guard must evaluate to true");
}

#[test]
fn ironcrew_mode_guard_pattern_works_for_chat() {
    let lua = create_crew_lua().unwrap();
    set_ironcrew_mode(&lua, "chat").unwrap();
    let should_run: bool = lua
        .load(r#"return IRONCREW_MODE ~= "chat""#)
        .eval()
        .unwrap();
    assert!(
        !should_run,
        "in 'chat' mode the guard must evaluate to false"
    );
}
