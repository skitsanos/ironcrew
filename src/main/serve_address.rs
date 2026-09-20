use crate::utils::error::{IronCrewError, Result};
use std::env;

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct ServeEnvironment {
    pub(super) host: Option<String>,
    pub(super) ironcrew_port: Option<String>,
    pub(super) platform_port: Option<String>,
}

impl ServeEnvironment {
    pub(super) fn from_process() -> Result<Self> {
        Ok(Self {
            host: read_optional_env("IRONCREW_HOST")?,
            ironcrew_port: read_optional_env("IRONCREW_PORT")?,
            platform_port: read_optional_env("PORT")?,
        })
    }
}

fn read_optional_env(name: &str) -> Result<Option<String>> {
    match env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(env::VarError::NotUnicode(_)) => Err(IronCrewError::Validation(format!(
            "{name} must contain valid UTF-8"
        ))),
    }
}

fn parse_port(name: &str, value: &str) -> Result<u16> {
    let port = value.parse::<u16>().map_err(|_| {
        IronCrewError::Validation(format!(
            "{name} must be an integer between 1 and 65535, got {value:?}"
        ))
    })?;
    if port == 0 {
        return Err(IronCrewError::Validation(format!(
            "{name} must be between 1 and 65535, got 0"
        )));
    }
    Ok(port)
}

/// Resolve server binding without making container-only defaults leak into the
/// local CLI. Explicit flags take precedence over IronCrew-specific variables,
/// which take precedence over Railway's conventional `PORT` variable.
pub(super) fn resolve_serve_address(
    cli_host: Option<String>,
    cli_port: Option<u16>,
    environment: ServeEnvironment,
) -> Result<(String, u16)> {
    let platform_port_is_set = environment.platform_port.is_some();

    let host = cli_host.or(environment.host).unwrap_or_else(|| {
        if platform_port_is_set {
            "0.0.0.0".to_owned()
        } else {
            "127.0.0.1".to_owned()
        }
    });
    if host.trim().is_empty() {
        return Err(IronCrewError::Validation(
            "server host must not be empty".into(),
        ));
    }

    let port = match cli_port {
        Some(0) => {
            return Err(IronCrewError::Validation(
                "--port must be between 1 and 65535, got 0".into(),
            ));
        }
        Some(port) => port,
        None => match environment.ironcrew_port {
            Some(value) => parse_port("IRONCREW_PORT", &value)?,
            None => match environment.platform_port {
                Some(value) => parse_port("PORT", &value)?,
                None => 3000,
            },
        },
    };

    Ok((host, port))
}
