use std::path::PathBuf;
use std::time::Duration;

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
