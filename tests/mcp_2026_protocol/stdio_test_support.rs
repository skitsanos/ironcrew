use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;
use tempfile::TempDir;

pub(super) async fn wait_for_file(path: &Path) {
    for _ in 0..80 {
        if path.exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for {}", path.display());
}

pub(super) async fn wait_for_request(temp: &TempDir, name: Option<&str>) {
    let method = match name {
        Some("__discover__") => "server/discover",
        Some(_) => "tools/call",
        None => "tools/list",
    };
    for _ in 0..80 {
        if read_log(temp).iter().any(|entry| {
            entry["method"] == method
                && name.is_none_or(|name| name == "__discover__" || entry["name"] == name)
        }) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for fixture request");
}

fn read_log(temp: &TempDir) -> Vec<Value> {
    std::fs::read_to_string(temp.path().join("requests.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

pub(super) fn call_count(temp: &TempDir, name: &str) -> usize {
    read_log(temp)
        .iter()
        .filter(|entry| entry["method"] == "tools/call" && entry["name"] == name)
        .count()
}

#[cfg(unix)]
pub(super) async fn assert_process_stopped(pid_file: PathBuf) {
    let pid = std::fs::read_to_string(&pid_file)
        .unwrap()
        .parse::<i32>()
        .unwrap();
    let pid = nix::unistd::Pid::from_raw(pid);
    for _ in 0..80 {
        if matches!(
            nix::sys::signal::kill(pid, None),
            Err(nix::errno::Errno::ESRCH)
        ) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!(
        "fixture process {pid} is still alive ({})",
        pid_file.display()
    );
}
