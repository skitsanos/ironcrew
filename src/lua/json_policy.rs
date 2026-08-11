//! Captured resource bounds for Lua/JSON conversion.

const DEFAULT_MAX_DEPTH: usize = 64;
const DEFAULT_MAX_NODES: usize = 100_000;
const DEFAULT_MAX_STRING_BYTES: usize = 8 * 1024 * 1024;
const DEFAULT_MAX_OUTPUT_BYTES: usize = 16 * 1024 * 1024;

const MAX_DEPTH_CEILING: usize = 256;
const MAX_NODES_CEILING: usize = 1_000_000;
const MAX_STRING_BYTES_CEILING: usize = 256 * 1024 * 1024;
const MAX_OUTPUT_BYTES_CEILING: usize = 256 * 1024 * 1024;

#[derive(Clone, Copy, Debug)]
pub(crate) struct JsonLimits {
    pub(crate) max_depth: usize,
    pub(crate) max_nodes: usize,
    pub(crate) max_string_bytes: usize,
    pub(crate) max_output_bytes: usize,
}

impl Default for JsonLimits {
    fn default() -> Self {
        Self {
            max_depth: DEFAULT_MAX_DEPTH,
            max_nodes: DEFAULT_MAX_NODES,
            max_string_bytes: DEFAULT_MAX_STRING_BYTES,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
        }
    }
}

impl JsonLimits {
    pub(crate) fn from_env() -> mlua::Result<Self> {
        Ok(Self {
            max_depth: bounded_env(
                "IRONCREW_LUA_JSON_MAX_DEPTH",
                DEFAULT_MAX_DEPTH,
                1,
                MAX_DEPTH_CEILING,
            )?,
            max_nodes: bounded_env(
                "IRONCREW_LUA_JSON_MAX_NODES",
                DEFAULT_MAX_NODES,
                1,
                MAX_NODES_CEILING,
            )?,
            max_string_bytes: bounded_env(
                "IRONCREW_LUA_JSON_MAX_STRING_BYTES",
                DEFAULT_MAX_STRING_BYTES,
                1,
                MAX_STRING_BYTES_CEILING,
            )?,
            max_output_bytes: bounded_env(
                "IRONCREW_LUA_JSON_MAX_OUTPUT_BYTES",
                DEFAULT_MAX_OUTPUT_BYTES,
                1,
                MAX_OUTPUT_BYTES_CEILING,
            )?,
        })
    }

    pub(crate) fn definition(self) -> serde_json::Value {
        serde_json::json!({
            "max_depth": self.max_depth,
            "max_nodes": self.max_nodes,
            "max_string_bytes": self.max_string_bytes,
            "max_output_bytes": self.max_output_bytes,
        })
    }

    #[cfg(test)]
    pub(crate) fn from_marker(marker: usize) -> Self {
        Self {
            max_depth: marker,
            max_nodes: marker,
            max_string_bytes: marker,
            max_output_bytes: marker,
        }
    }
}

fn bounded_env(name: &str, default: usize, min: usize, max: usize) -> mlua::Result<usize> {
    let raw = match std::env::var(name) {
        Ok(raw) => raw,
        Err(std::env::VarError::NotPresent) => return Ok(default),
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err(mlua::Error::external(format!(
                "{name} must contain valid Unicode digits"
            )));
        }
    };
    let value = raw.parse::<usize>().map_err(|_| {
        mlua::Error::external(format!(
            "{name} must be a whole number between {min} and {max}"
        ))
    })?;
    if !(min..=max).contains(&value) {
        return Err(mlua::Error::external(format!(
            "{name} must be between {min} and {max}; got {value}"
        )));
    }
    Ok(value)
}
