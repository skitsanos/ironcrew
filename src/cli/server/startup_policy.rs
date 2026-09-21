use super::*;

pub(super) const MAX_SHUTDOWN_TIMEOUT_SECS: u64 = 300;
pub(super) const MAX_SHUTDOWN_ROUTING_GRACE_SECS: u64 = 300;
pub(super) const MAX_SHUTDOWN_DRAIN_MS: u64 = 30_000;

pub(super) enum StartupPruneOutcome {
    Pruned(usize),
    MaintenanceUnhealthy(IronCrewError),
}

pub(super) async fn startup_prune_with_policy<F>(
    maintenance_watchdog: Option<std::time::Duration>,
    prune: F,
) -> Result<StartupPruneOutcome>
where
    F: std::future::Future<Output = Result<usize>>,
{
    let prune_result = match maintenance_watchdog {
        Some(timeout) => match tokio::time::timeout(timeout, prune).await {
            Ok(result) => result,
            Err(_) => Err(IronCrewError::Validation(format!(
                "Startup idempotency pruning exceeded its {} ms maintenance timeout",
                timeout.as_millis()
            ))),
        },
        None => prune.await,
    };
    match prune_result {
        Ok(count) => Ok(StartupPruneOutcome::Pruned(count)),
        Err(error) => {
            crate::metrics::record_store_error(crate::metrics::StoreOperation::Idempotency, &error);
            if maintenance_watchdog.is_some() {
                Ok(StartupPruneOutcome::MaintenanceUnhealthy(error))
            } else {
                Err(IronCrewError::Validation(format!(
                    "Failed to prune the idempotency ledger at startup: {error}"
                )))
            }
        }
    }
}

pub(super) fn bounded_env_u64(name: &str, default: u64, min: u64, max: u64) -> Result<u64> {
    let value = match std::env::var(name) {
        Ok(value) => value.parse::<u64>().map_err(|_| {
            IronCrewError::Validation(format!("{name} must be an integer between {min} and {max}"))
        })?,
        Err(std::env::VarError::NotPresent) => default,
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err(IronCrewError::Validation(format!(
                "{name} must contain valid UTF-8"
            )));
        }
    };
    if !(min..=max).contains(&value) {
        return Err(IronCrewError::Validation(format!(
            "{name} must be between {min} and {max}"
        )));
    }
    Ok(value)
}

pub(super) fn public_bind_requires_auth(host: &str) -> bool {
    host.parse::<std::net::IpAddr>()
        .map(|address| !address.is_loopback())
        .unwrap_or_else(|_| !host.eq_ignore_ascii_case("localhost"))
}

pub(super) fn unauthenticated_public_bind_allowed() -> Result<bool> {
    match std::env::var("IRONCREW_ALLOW_UNAUTHENTICATED") {
        Err(std::env::VarError::NotPresent) => Ok(false),
        Ok(value) if value == "1" || value.eq_ignore_ascii_case("true") => Ok(true),
        Ok(value) if value == "0" || value.eq_ignore_ascii_case("false") => Ok(false),
        Ok(_) | Err(std::env::VarError::NotUnicode(_)) => Err(IronCrewError::Validation(
            "IRONCREW_ALLOW_UNAUTHENTICATED must be one of: 1, true, 0, false".into(),
        )),
    }
}

pub(super) fn prepare_file_write_root(public_bind: bool, flows_dir: &Path) -> Result<()> {
    let configured = std::env::var_os("IRONCREW_FILE_WRITE_ROOT");
    let Some(configured) = configured.filter(|value| !value.is_empty()) else {
        if public_bind {
            return Err(IronCrewError::Validation(
                "Public server binds require IRONCREW_FILE_WRITE_ROOT to be an explicit writable directory separate from the flow source tree".into(),
            ));
        }
        return Ok(());
    };
    let root = std::path::PathBuf::from(configured);
    if public_bind && !root.is_absolute() {
        return Err(IronCrewError::Validation(
            "IRONCREW_FILE_WRITE_ROOT must be absolute for public server binds".into(),
        ));
    }
    std::fs::create_dir_all(&root).map_err(|error| {
        IronCrewError::Validation(format!(
            "Failed to create IRONCREW_FILE_WRITE_ROOT '{}': {error}",
            root.display()
        ))
    })?;
    let root = std::fs::canonicalize(&root).map_err(|error| {
        IronCrewError::Validation(format!(
            "Failed to resolve IRONCREW_FILE_WRITE_ROOT '{}': {error}",
            root.display()
        ))
    })?;
    if root == flows_dir || root.starts_with(flows_dir) || flows_dir.starts_with(&root) {
        return Err(IronCrewError::Validation(format!(
            "IRONCREW_FILE_WRITE_ROOT '{}' must be disjoint from flows directory '{}'",
            root.display(),
            flows_dir.display()
        )));
    }
    Ok(())
}

pub(super) fn require_public_mcp_policy(public_bind: bool) -> Result<()> {
    if !public_bind {
        return Ok(());
    }
    for (name, transport) in [
        ("IRONCREW_MCP_ALLOWED_COMMANDS", "stdio"),
        ("IRONCREW_MCP_ALLOWED_HTTP_HOSTS", "HTTP"),
    ] {
        if !matches!(std::env::var(name), Ok(value) if !value.trim().is_empty()) {
            return Err(IronCrewError::Validation(format!(
                "Public server binds require {name}; set an exact allowlist or __disabled__ to disable {transport} MCP"
            )));
        }
    }
    Ok(())
}
