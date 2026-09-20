use super::*;
use std::process::Stdio;
use std::time::{Duration, Instant};

#[test]
fn option_like_project_path_cannot_turn_validation_into_help_success() {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("--help");
    std::fs::create_dir(&project).unwrap();
    std::fs::write(
        project.join("crew.lua"),
        "Crew.new({goal='test',modle='typo'})",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ironcrew"))
        .current_dir(root.path())
        .args(["validate", "--evaluate", "--", "--help"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{}", text(&output));
    assert!(text(&output).contains("modle"));
}

// Pathological native pattern matching does not yield to Lua's VM hook.
const NATIVE_WORK: &str =
    "local s=string.rep('a',30000); string.find(s,'a*a*a*a*a*b'); fs.write('marker','bad')";

#[test]
fn native_work_is_killed_at_the_hard_deadline() {
    let dir = fixture(NATIVE_WORK);
    let start = Instant::now();
    let output = validate(dir.path());
    assert_eq!(output.status.code(), Some(1), "{}", text(&output));
    assert!(text(&output).contains("hard deadline"), "{}", text(&output));
    assert!(start.elapsed() < Duration::from_secs(15));
    assert!(!dir.path().join("marker").exists());
}

#[test]
fn killing_supervisor_terminates_worker_and_leaves_no_effects() {
    let dir = fixture(NATIVE_WORK);
    let mut parent = Command::new(env!("CARGO_BIN_EXE_ironcrew"))
        .args(["validate", "--evaluate"])
        .arg(dir.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let worker = loop {
        let result = Command::new("ps")
            .args(["-axo", "pid=,ppid="])
            .output()
            .unwrap();
        let parent_id = parent.id().to_string();
        if let Some(pid) = String::from_utf8_lossy(&result.stdout)
            .lines()
            .find_map(|line| {
                let mut fields = line.split_whitespace();
                let pid = fields.next()?;
                (fields.next()? == parent_id).then(|| pid.to_owned())
            })
        {
            break pid;
        }
        if Instant::now() >= deadline {
            let _ = parent.kill();
            let _ = parent.wait();
            panic!("validation worker did not start");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    // Give the exec'd worker time to install its lifetime-pipe watcher.
    std::thread::sleep(Duration::from_millis(100));
    parent.kill().unwrap();
    parent.wait().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let result = Command::new("ps")
            .args(["-o", "stat=", "-p", &worker])
            .output()
            .unwrap();
        let state = String::from_utf8_lossy(&result.stdout);
        if state.trim().is_empty() || state.trim().starts_with('Z') {
            break;
        }
        if Instant::now() >= deadline {
            let _ = Command::new("kill").args(["-KILL", &worker]).status();
            panic!("validation worker survived supervisor cancellation");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!dir.path().join("marker").exists());
    assert!(!dir.path().join(".ironcrew").exists());
}
