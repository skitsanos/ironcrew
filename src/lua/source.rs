//! Bounded loading for every Lua source file executed by IronCrew.

use std::fs::File;
use std::io::{Read, Take};
use std::path::Path;

use crate::utils::error::{IronCrewError, Result};

const DEFAULT_MAX_SOURCE_BYTES: usize = 1024 * 1024;
const MAX_SOURCE_BYTES_CEILING: usize = 16 * 1024 * 1024;

fn configured_source_limit() -> Result<usize> {
    let raw = match std::env::var("IRONCREW_LUA_MAX_SOURCE_BYTES") {
        Ok(raw) => raw,
        Err(std::env::VarError::NotPresent) => return Ok(DEFAULT_MAX_SOURCE_BYTES),
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err(IronCrewError::Validation(
                "IRONCREW_LUA_MAX_SOURCE_BYTES must contain valid Unicode digits".into(),
            ));
        }
    };
    let value = raw.parse::<usize>().map_err(|_| {
        IronCrewError::Validation(format!(
            "IRONCREW_LUA_MAX_SOURCE_BYTES must be a whole number between 1 and {MAX_SOURCE_BYTES_CEILING}"
        ))
    })?;
    if !(1..=MAX_SOURCE_BYTES_CEILING).contains(&value) {
        return Err(IronCrewError::Validation(format!(
            "IRONCREW_LUA_MAX_SOURCE_BYTES must be between 1 and {MAX_SOURCE_BYTES_CEILING}; got {value}"
        )));
    }
    Ok(value)
}

fn read_bounded(file: File, path: &Path, limit: usize) -> Result<Vec<u8>> {
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(IronCrewError::Validation(format!(
            "Lua source path is not a regular file: {}",
            path.display()
        )));
    }
    if metadata.len() > limit as u64 {
        return Err(source_too_large(path, limit));
    }

    // Metadata is only an early rejection. The bounded read is authoritative
    // if the file grows (or is replaced) after the metadata check.
    let mut reader: Take<File> = file.take(limit as u64 + 1);
    let mut bytes = Vec::with_capacity((metadata.len() as usize).min(limit));
    reader.read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(source_too_large(path, limit));
    }
    Ok(bytes)
}

fn source_too_large(path: &Path, limit: usize) -> IronCrewError {
    IronCrewError::Validation(format!(
        "Lua source '{}' exceeds the configured limit of {} bytes",
        path.display(),
        limit
    ))
}

fn read_lua_source_with_limit(path: &Path, limit: usize) -> Result<String> {
    let file = File::open(path)?;
    let bytes = read_bounded(file, path, limit)?;
    String::from_utf8(bytes).map_err(|error| {
        IronCrewError::Validation(format!(
            "Lua source '{}' is not valid UTF-8: {}",
            path.display(),
            error.utf8_error()
        ))
    })
}

/// Read a Lua source file with an operator-configurable 1 MiB default limit.
///
/// The file handle is opened before metadata is inspected, and the actual read
/// is independently bounded to protect against a concurrent file-size change.
pub fn read_lua_source(path: &Path) -> Result<String> {
    read_lua_source_with_limit(path, configured_source_limit()?)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn accepts_source_at_exact_limit() {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("crew.lua");
        fs::write(&path, "return 1").unwrap();

        assert_eq!(read_lua_source_with_limit(&path, 8).unwrap(), "return 1");
    }

    #[test]
    fn rejects_source_over_limit_before_unbounded_allocation() {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("crew.lua");
        fs::write(&path, "return 12").unwrap();

        let error = read_lua_source_with_limit(&path, 8).unwrap_err();
        assert!(error.to_string().contains("exceeds the configured limit"));
    }

    #[test]
    fn rejects_invalid_utf8() {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("crew.lua");
        fs::write(&path, [0xff, 0xfe]).unwrap();

        let error = read_lua_source_with_limit(&path, 8).unwrap_err();
        assert!(error.to_string().contains("not valid UTF-8"));
    }

    #[test]
    fn rejects_directories() {
        let directory = TempDir::new().unwrap();
        let error = read_lua_source_with_limit(directory.path(), 8).unwrap_err();
        assert!(error.to_string().contains("not a regular file"));
    }
}
