use tracing_subscriber::{fmt, EnvFilter};

pub fn init(verbose: bool) {
    let filter = if verbose {
        EnvFilter::new("debug")
    } else {
        EnvFilter::try_from_env("IRONCREW_LOG").unwrap_or_else(|_| EnvFilter::new("info"))
    };

    fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}
