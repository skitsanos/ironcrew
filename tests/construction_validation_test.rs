//! Process-level, credential-free construction validation acceptance.
#![cfg(unix)]
use std::process::{Command, Output};

#[path = "construction_validation/process.rs"]
mod process;

const CREW: &str = "local c = Crew.new({goal='test'})\nc:add_agent({name='a',goal='work'})\n";

#[test]
fn mcp_discovery_is_incomplete_without_spawning_its_command() {
    let dir = fixture("");
    let marker = dir.path().join("marker");
    let quoted = serde_json::to_string(&marker.to_string_lossy()).unwrap();
    std::fs::write(dir.path().join("crew.lua"), format!("local c=Crew.new({{goal='test',mcp_servers={{probe={{transport='stdio',execution_identity='test',command='/usr/bin/touch',args={{{quoted}}}}}}}}}); c:add_agent({{name='a',goal='work'}})")).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ironcrew"))
        .args(["validate", "--evaluate"])
        .arg(dir.path())
        .env("IRONCREW_MCP_ALLOWED_COMMANDS", "/usr/bin/touch")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(3), "{}", text(&output));
    assert!(text(&output).contains("MCP"));
    assert!(!marker.exists());
}

fn validate(dir: &std::path::Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ironcrew"))
        .args(["validate", "--evaluate"])
        .arg(dir)
        .env_remove("OPENAI_API_KEY")
        .env_remove("ANTHROPIC_API_KEY")
        .env("IRONCREW_LOG", "error")
        .output()
        .unwrap()
}

fn fixture(source: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("crew.lua"), source).unwrap();
    dir
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn valid_construction_needs_no_key_and_creates_no_store() {
    let dir = fixture(&format!(
        "{CREW}c:add_task({{name='t',description='work',agent='a'}})"
    ));
    let output = validate(dir.path());
    assert!(output.status.success(), "{}", text(&output));
    assert!(text(&output).contains("Construction validation PASSED"));
    assert!(!dir.path().join(".ironcrew").exists());
    let output = Command::new(env!("CARGO_BIN_EXE_ironcrew"))
        .current_dir(dir.path())
        .args(["validate", "--evaluate", "crew.lua"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", text(&output));
}

#[test]
fn instruction_budget_is_shared_across_declaration_vms() {
    let dir = fixture("Crew.new({goal='test'})");
    std::fs::create_dir(dir.path().join("agents")).unwrap();
    for i in 0..8 {
        std::fs::write(
            dir.path().join(format!("agents/a{i}.lua")),
            format!("local n=0; for i=1,180000 do n=n+i end; return {{name='a{i}',goal='work'}}"),
        )
        .unwrap();
    }
    let output = validate(dir.path());
    assert_eq!(output.status.code(), Some(1), "{}", text(&output));
    assert!(
        text(&output).contains("instruction/time limit"),
        "{}",
        text(&output)
    );
}

#[test]
fn real_constructors_and_dependency_graph_reject_invalid_inputs() {
    for source in [
        "Crew.new({goal='test', modle='typo'})".to_owned(),
        "local c=Crew.new({goal='test'}); c:add_agent({name='a',goal='work',reasoning_effort='minimal'})".to_owned(),
        format!("{CREW}c:add_task({{name='t',description='work',depends_on={{'missing'}}}})"),
        format!("{CREW}c:add_task({{name='t',description='work',depends_on={{'t'}}}})"),
        format!("{CREW}c:conversation({{agent='a',max_histroy=1}})"),
        format!("{CREW}c:dialog({{agents={{'a','missing'}},starter='hi'}})"),
        format!("{CREW}c:add_task({{name='t',description='work',agent='missing'}})"),
        format!("{CREW}c:add_task({{name='t',description='work',on_error='missing'}})"),
        "local c=Crew.new({goal='test',model='custom'}); c:add_agent({name='a',goal='work',temperature=0.5}); c:add_task({name='t',description='work',agent='a',model='gpt-5.6-luna'})".into(),
    ] {
        let dir = fixture(&source);
        let output = validate(dir.path());
        assert_eq!(output.status.code(), Some(1), "{source}: {}", text(&output));
        assert!(!dir.path().join(".ironcrew").exists());
    }
}

#[test]
fn declared_callbacks_are_not_invoked_but_direct_calls_remain_sandboxed() {
    let dir = fixture(
        "local c=Crew.new({goal='test'}); c:add_agent({name='a',goal='work'}); c:add_agent({name='b',goal='work'}); local callback=function() fs.write('marker','bad') end; c:dialog({agents={'a','b'},starter='hi',should_stop=callback})",
    );
    let output = validate(dir.path());
    assert!(output.status.success(), "{}", text(&output));
    assert!(text(&output).contains("Callback bodies"));
    assert!(!dir.path().join("marker").exists());
    let source = std::fs::read_to_string(dir.path().join("crew.lua")).unwrap();
    std::fs::write(dir.path().join("crew.lua"), format!("{source}; callback()")).unwrap();
    let output = validate(dir.path());
    assert_eq!(output.status.code(), Some(3), "{}", text(&output));
    assert!(!dir.path().join("marker").exists());
}

#[test]
fn dotenv_and_stored_memory_are_not_read_to_construct_options() {
    let dir = fixture(
        "local c=Crew.new({goal='test',provider='openai-responses'}); c:add_agent({name='a',goal='work'})",
    );
    // A normal command would load this invalid environment setting. Strict
    // evaluation must not import .env or require any provider credential.
    std::fs::write(
        dir.path().join(".env"),
        "IRONCREW_MAX_AGENTS=not-an-integer\n",
    )
    .unwrap();
    let output = validate(dir.path());
    assert!(output.status.success(), "{}", text(&output));
    std::fs::write(
        dir.path().join("crew.lua"),
        format!("{CREW}c:memory_get('prior')"),
    )
    .unwrap();
    let output = validate(dir.path());
    assert_eq!(output.status.code(), Some(3), "{}", text(&output));
}

#[test]
fn execution_boundary_is_incomplete_and_never_returns_fake_results() {
    let dir = fixture(&format!(
        "{CREW}local results=c:run(); error('fabricated results')"
    ));
    let output = validate(dir.path());
    assert_eq!(output.status.code(), Some(3), "{}", text(&output));
    assert!(text(&output).contains("INCOMPLETE"));
    assert!(!text(&output).contains("fabricated results"));
    assert!(!dir.path().join(".ironcrew").exists());
}

#[test]
fn effects_are_blocked_in_every_project_loading_phase() {
    use std::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    for effect in [
        format!("http.get('{url}')"),
        "fs.write('marker', 'bad')".into(),
        "os.execute('touch marker')".into(),
        "io.open('marker','w')".into(),
        "postgres.execute('write')".into(),
        "llm.chat('request')".into(),
        "run_flow('nested.lua')".into(),
        "env('OPENAI_API_KEY')".into(),
        "pcall(function() fs.write('marker','bad') end)".into(),
        "setmetatable({}, {__gc=function() fs.write('marker','bad') end})".into(),
    ] {
        for phase in [
            "crew.lua",
            "config.lua",
            "agents/a.lua",
            "tools/t.lua",
            "_lib/value.lua",
        ] {
            let dir = fixture(&format!("{CREW}require('value')"));
            std::fs::create_dir_all(dir.path().join("_lib")).unwrap();
            std::fs::write(dir.path().join("_lib/value.lua"), "return true").unwrap();
            let file = dir.path().join(phase);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, &effect).unwrap();
            let output = validate(dir.path());
            assert_eq!(
                output.status.code(),
                Some(3),
                "{phase} {effect}: {}",
                text(&output)
            );
            assert!(!dir.path().join("marker").exists());
            assert!(!dir.path().join(".ironcrew").exists());
            assert!(listener.accept().is_err(), "unexpected network connection");
        }
    }
}

#[test]
fn provider_and_session_execution_never_connect() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!(
        "local c=Crew.new({{goal='test',base_url='http://{}/v1',api_key='inert'}}); c:add_agent({{name='a',goal='work'}}); c:add_agent({{name='b',goal='work'}});",
        listener.local_addr().unwrap()
    );
    for call in [
        "c:add_task({name='t',description='work'}); c:run()",
        "local s=c:conversation({agent='a'}); s:send('hi')",
        "local s=c:dialog({agents={'a','b'},starter='hi'}); s:run()",
        "c:ask_human({prompt='hi'})",
        "c:subworkflow('nested.lua')",
    ] {
        let dir = fixture(&format!("{base}{call}"));
        let output = validate(dir.path());
        assert_eq!(output.status.code(), Some(3), "{call}: {}", text(&output));
        assert!(listener.accept().is_err());
        assert!(!dir.path().join(".ironcrew").exists());
    }
}

#[test]
fn budgets_and_recursion_fail_closed() {
    for source in [
        "while true do end",
        "local function recurse() return 1 + recurse() end; recurse()",
        "local x = string.rep('x', 64*1024*1024)",
        "local c = Crew.new({goal='test'}); for i=1,300 do c:add_agent({name='a'..i,goal='work'}) end",
    ] {
        let dir = fixture(source);
        let start = std::time::Instant::now();
        let output = validate(dir.path());
        assert_eq!(output.status.code(), Some(1), "{source}: {}", text(&output));
        assert!(start.elapsed() < std::time::Duration::from_secs(15));
        assert!(!dir.path().join(".ironcrew").exists());
    }
}

#[test]
fn persistent_construction_and_sessions_do_not_open_storage() {
    let dir = fixture(
        "local c=Crew.new({goal='test',provider='openai-responses',memory='persistent'}); c:add_agent({name='a',goal='work'}); c:add_agent({name='b',goal='work'}); c:conversation({agent='a',id='session'}); c:dialog({agents={'a','b'},starter='hi',id='dialog'})",
    );
    // A store open would fail on this sentinel rather than being harmless.
    std::fs::write(dir.path().join(".ironcrew"), "untouched").unwrap();
    let output = validate(dir.path());
    assert!(output.status.success(), "{}", text(&output));
    assert_eq!(
        std::fs::read_to_string(dir.path().join(".ironcrew")).unwrap(),
        "untouched"
    );
}

#[test]
fn snapshot_imports_work_and_symlinks_are_rejected() {
    let dir = fixture(
        "local options=require('options'); local c=Crew.new(options); c:add_agent({name='a',goal='work'})",
    );
    std::fs::create_dir(dir.path().join("_lib")).unwrap();
    std::fs::write(dir.path().join("_lib/options.lua"), "return {goal='test'}").unwrap();
    let output = validate(dir.path());
    assert!(output.status.success(), "{}", text(&output));
    let outside = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), dir.path().join("outside")).unwrap();
    let output = validate(dir.path());
    assert_eq!(output.status.code(), Some(1), "{}", text(&output));
    assert!(text(&output).contains("symlink"));
}
