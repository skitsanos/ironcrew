//! Process isolation provides the hard deadline even inside a native Lua
//! function (such as string pattern matching), where instruction hooks cannot
//! preempt execution. Dropping the supervisor also kills the worker.
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

use crate::utils::error::{IronCrewError, Result};

const MAX_OUTPUT_BYTES: u64 = 64 * 1024;

pub(super) async fn run(path: &Path) -> Result<()> {
    let mut child = Command::new(std::env::current_exe()?)
        .args(["validate", "--evaluate", "--construction-worker"])
        .arg("--")
        .arg(path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    // Child::wait closes its own stdin; retain it outside Child until this
    // supervisor finishes. EOF tells the worker that its parent disappeared.
    let _lifetime = child.stdin.take().expect("piped lifetime channel");
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let output = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::try_join!(child.wait(), bounded_output(stdout), bounded_output(stderr))
    })
    .await;
    let (status, stdout, stderr) = match output {
        Ok(Ok(result)) => result,
        result => {
            // Kill and reap before reporting a timeout/read failure. There is
            // no detached evaluation left running after a failed CLI command.
            let _ = child.kill().await;
            return Err(IronCrewError::Validation(match result {
                Err(_) => "construction evaluation exceeded the 10-second hard deadline".into(),
                Ok(Err(error)) => format!("construction worker output failed: {error}"),
                Ok(Ok(_)) => unreachable!(),
            }));
        }
    };
    if status.success() {
        print!("{stdout}");
        return Ok(());
    }
    let reason = if stderr.trim().is_empty() {
        "construction worker terminated".into()
    } else {
        stderr
    };
    if status.code() == Some(3) {
        Err(IronCrewError::ValidationIncomplete(reason))
    } else {
        Err(IronCrewError::Validation(reason))
    }
}

async fn bounded_output(reader: impl AsyncRead + Unpin) -> std::io::Result<String> {
    let mut bytes = Vec::new();
    reader
        .take(MAX_OUTPUT_BYTES + 1)
        .read_to_end(&mut bytes)
        .await?;
    if bytes.len() as u64 > MAX_OUTPUT_BYTES {
        return Err(std::io::Error::other("construction output exceeds 64 KiB"));
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}
