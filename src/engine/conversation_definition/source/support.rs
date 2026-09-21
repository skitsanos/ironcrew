use std::path::Path;

use cap_std::fs::{Metadata, MetadataExt};

use super::super::validation;
use crate::utils::error::{IronCrewError, Result};

const DEFAULT_FILE_BYTES: usize = 1024 * 1024;
const MAX_FILE_BYTES: usize = 16 * 1024 * 1024;

pub(super) fn same_object(left: &Metadata, right: &Metadata) -> bool {
    left.dev() == right.dev() && left.ino() == right.ino()
}

pub(super) fn same_snapshot(left: &Metadata, right: &Metadata) -> bool {
    same_object(left, right)
        && left.len() == right.len()
        && left.mtime() == right.mtime()
        && left.mtime_nsec() == right.mtime_nsec()
        && left.ctime() == right.ctime()
        && left.ctime_nsec() == right.ctime_nsec()
}

pub(super) fn configured_file_limit() -> Result<usize> {
    let raw = match std::env::var("IRONCREW_LUA_MAX_SOURCE_BYTES") {
        Ok(raw) => raw,
        Err(std::env::VarError::NotPresent) => return Ok(DEFAULT_FILE_BYTES),
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err(validation(
                "IRONCREW_LUA_MAX_SOURCE_BYTES must contain valid Unicode digits",
            ));
        }
    };
    let value = raw.parse::<usize>().map_err(|_| {
        validation(format!(
            "IRONCREW_LUA_MAX_SOURCE_BYTES must be a whole number between 1 and {MAX_FILE_BYTES}"
        ))
    })?;
    if !(1..=MAX_FILE_BYTES).contains(&value) {
        return Err(validation(format!(
            "IRONCREW_LUA_MAX_SOURCE_BYTES must be between 1 and {MAX_FILE_BYTES}; got {value}"
        )));
    }
    Ok(value)
}

pub(super) fn invalid_lua_path(path: &Path) -> IronCrewError {
    validation(format!(
        "flow source must be a regular file, not a symlink or special file: {}",
        path.display()
    ))
}

pub(super) fn changed_path(path: &Path) -> IronCrewError {
    validation(format!(
        "flow source changed or became unsafe while hashing: {}",
        path.display()
    ))
}

pub(super) fn source_too_large(path: &Path, limit: usize) -> IronCrewError {
    validation(format!(
        "flow source '{}' exceeds the configured limit of {} bytes",
        path.display(),
        limit
    ))
}
