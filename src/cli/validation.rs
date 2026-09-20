use std::path::Path;

use crate::utils::error::Result;

pub async fn cmd_evaluate(path: &Path, worker: bool) -> Result<()> {
    if !worker {
        return supervisor::run(path).await;
    }
    // A dedicated thread watches the parent's lifetime pipe, including when
    // the parent is forcibly killed and cannot run Rust Drop implementations.
    // It is not a Tokio blocking task, so it cannot delay runtime shutdown.
    std::thread::spawn(|| {
        use std::io::Read;
        let _ = std::io::stdin().read(&mut [0_u8; 1]);
        std::process::exit(1);
    });
    let report = crate::lua::construction::evaluate(path).await?;
    println!(
        "Construction validation PASSED: {} crew(s), {} agent(s), {} task(s).",
        report.crews, report.agents, report.tasks
    );
    println!(
        "Evaluated construction only. Callback bodies, untaken branches, credentials, external state and runtime behavior are not validated."
    );
    Ok(())
}

mod supervisor;
