const MAX_ENV_NAME_BYTES: usize = 256;

/// Read a process environment variable only when its exact name is present
/// in `IRONCREW_ENV_ALLOWLIST`. This is the shared policy for Lua `env()` and
/// `${env.NAME}` interpolation so neither path can bypass the other.
pub fn read_allowlisted(name: &str) -> Option<String> {
    if name.is_empty()
        || name.len() > MAX_ENV_NAME_BYTES
        || name
            .chars()
            .any(|character| character == '=' || character == '\0' || character.is_control())
    {
        return None;
    }

    let allowlist = std::env::var("IRONCREW_ENV_ALLOWLIST").ok()?;
    if !is_name_allowlisted(name, &allowlist) {
        return None;
    }
    std::env::var(name).ok()
}

fn is_name_allowlisted(name: &str, allowlist: &str) -> bool {
    allowlist
        .split(',')
        .map(str::trim)
        .any(|allowed| !allowed.is_empty() && allowed.eq_ignore_ascii_case(name))
}

#[cfg(test)]
mod tests {
    use super::is_name_allowlisted;

    #[test]
    fn allowlist_is_exact_case_insensitive_and_ignores_empty_entries() {
        assert!(is_name_allowlisted(
            "APP_REGION",
            " feature_flag, app_region ,,"
        ));
        assert!(!is_name_allowlisted("APP", "APP_REGION"));
        assert!(!is_name_allowlisted("APP_REGION_EXTRA", "APP_REGION"));
        assert!(!is_name_allowlisted("APP_REGION", ",,,"));
    }
}
