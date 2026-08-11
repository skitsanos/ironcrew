//! Captured, secret-free process policy used by durable conversation tools.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::utils::error::{IronCrewError, Result};

const DEFAULT_LUA_FS_MAX_BYTES: usize = 1024 * 1024;
const HARD_LUA_FS_MAX_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug)]
pub(crate) struct CapabilityRoot {
    resolved: std::result::Result<PathBuf, &'static str>,
    fingerprint: std::result::Result<String, &'static str>,
}

impl CapabilityRoot {
    pub(crate) fn required(path: Option<PathBuf>) -> Self {
        let resolved = path.map(Ok).unwrap_or_else(|| {
            std::env::current_dir().map_err(|_| "filesystem capability root is unavailable")
        });
        Self::capture(resolved)
    }

    pub(crate) fn optional(path: Option<PathBuf>) -> Option<Self> {
        path.map(|path| Self::capture(Ok(path)))
    }

    fn capture(path: std::result::Result<PathBuf, &'static str>) -> Self {
        let resolved = path.and_then(|path| {
            std::fs::canonicalize(path)
                .map_err(|_| "filesystem capability root could not be resolved")
        });
        let fingerprint = resolved
            .as_ref()
            .map(|path| path_fingerprint(path))
            .map_err(|error| *error);
        Self {
            resolved,
            fingerprint,
        }
    }

    pub(crate) fn path(&self) -> std::result::Result<&Path, &'static str> {
        self.resolved.as_deref().map_err(|error| *error)
    }

    pub(crate) fn cloned_path(&self) -> std::result::Result<PathBuf, &'static str> {
        self.path().map(Path::to_path_buf)
    }

    pub(crate) fn fingerprint(&self) -> Result<&str> {
        self.fingerprint
            .as_deref()
            .map_err(|message| IronCrewError::Validation((*message).into()))
    }
}

fn path_fingerprint(path: &Path) -> String {
    let mut digest = Sha256::new();
    frame(&mut digest, b"ironcrew:tool-capability-root:v1");
    frame(&mut digest, std::env::consts::OS.as_bytes());
    frame(&mut digest, &path_bytes(path));
    finish_digest(digest)
}

#[cfg(unix)]
fn path_bytes(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}

#[cfg(windows)]
fn path_bytes(path: &Path) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str()
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect()
}

#[cfg(not(any(unix, windows)))]
fn path_bytes(path: &Path) -> Vec<u8> {
    path.as_os_str().to_string_lossy().as_bytes().to_vec()
}

pub(crate) fn strings_fingerprint(label: &str, values: &[String]) -> String {
    let mut digest = Sha256::new();
    frame(&mut digest, b"ironcrew:tool-policy-strings:v1");
    frame(&mut digest, label.as_bytes());
    frame(&mut digest, &(values.len() as u64).to_be_bytes());
    for value in values {
        frame(&mut digest, value.as_bytes());
    }
    finish_digest(digest)
}

pub(crate) fn bytes_fingerprint(label: &str, value: &[u8]) -> String {
    let mut digest = Sha256::new();
    frame(&mut digest, b"ironcrew:tool-policy-bytes:v1");
    frame(&mut digest, label.as_bytes());
    frame(&mut digest, value);
    finish_digest(digest)
}

fn frame(digest: &mut Sha256, value: &[u8]) {
    digest.update((value.len() as u64).to_be_bytes());
    digest.update(value);
}

fn finish_digest(digest: Sha256) -> String {
    let bytes = digest.finalize();
    let mut output = String::with_capacity("sha256:".len() + bytes.len() * 2);
    output.push_str("sha256:");
    for byte in bytes {
        use std::fmt::Write;
        write!(&mut output, "{byte:02x}").expect("writing to String cannot fail");
    }
    output
}

pub(crate) fn strict_env_usize(
    name: &'static str,
    default: usize,
    min: usize,
    max: usize,
) -> std::result::Result<usize, String> {
    let Some(raw) = std::env::var(name).ok() else {
        return Ok(default);
    };
    let value = raw
        .parse::<usize>()
        .map_err(|_| format!("{name} must be an integer from {min} to {max}"))?;
    if !(min..=max).contains(&value) {
        return Err(format!("{name} must be from {min} to {max}"));
    }
    Ok(value)
}

#[derive(Clone, Debug)]
pub(crate) struct LuaToolPolicy {
    read_root: Option<CapabilityRoot>,
    write_root: Option<CapabilityRoot>,
    read_limit: std::result::Result<usize, String>,
    write_limit: std::result::Result<usize, String>,
}

impl LuaToolPolicy {
    pub(crate) fn capture(read_root: Option<PathBuf>, write_root: Option<PathBuf>) -> Self {
        let read_root = CapabilityRoot::optional(read_root);
        let write_root = CapabilityRoot::optional(write_root);
        let has_fs = read_root.is_some() || write_root.is_some();
        let capture_limit = |name| {
            if has_fs {
                strict_env_usize(name, DEFAULT_LUA_FS_MAX_BYTES, 1, HARD_LUA_FS_MAX_BYTES)
            } else {
                Ok(DEFAULT_LUA_FS_MAX_BYTES)
            }
        };
        Self {
            read_root,
            write_root,
            read_limit: capture_limit("IRONCREW_LUA_FS_MAX_READ_BYTES"),
            write_limit: capture_limit("IRONCREW_LUA_FS_MAX_WRITE_BYTES"),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_limits_for_test(
        read_root: Option<PathBuf>,
        write_root: Option<PathBuf>,
        read_limit: usize,
        write_limit: usize,
    ) -> Self {
        Self {
            read_root: CapabilityRoot::optional(read_root),
            write_root: CapabilityRoot::optional(write_root),
            read_limit: Ok(read_limit),
            write_limit: Ok(write_limit),
        }
    }

    pub(crate) fn roots(&self) -> std::result::Result<(Option<PathBuf>, Option<PathBuf>), String> {
        let read = self
            .read_root
            .as_ref()
            .map(CapabilityRoot::cloned_path)
            .transpose()
            .map_err(str::to_string)?;
        let write = self
            .write_root
            .as_ref()
            .map(CapabilityRoot::cloned_path)
            .transpose()
            .map_err(str::to_string)?;
        Ok((read, write))
    }

    pub(crate) fn limits(&self) -> std::result::Result<(usize, usize), String> {
        Ok((
            *self.read_limit.as_ref().map_err(Clone::clone)?,
            *self.write_limit.as_ref().map_err(Clone::clone)?,
        ))
    }

    pub(crate) fn definition(&self) -> Result<Value> {
        let read_root = self
            .read_root
            .as_ref()
            .map(CapabilityRoot::fingerprint)
            .transpose()?;
        let write_root = self
            .write_root
            .as_ref()
            .map(CapabilityRoot::fingerprint)
            .transpose()?;
        let (read_limit, write_limit) = self.limits().map_err(IronCrewError::Validation)?;
        Ok(json!({
            "read_root_fingerprint": read_root,
            "write_root_fingerprint": write_root,
            "max_read_bytes": read_limit,
            "max_write_bytes": write_limit,
        }))
    }
}
