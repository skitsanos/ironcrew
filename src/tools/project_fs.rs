//! Capability-scoped filesystem primitives used by project file tools.
//!
//! All path resolution happens relative to an already-open directory handle.
//! `cap-std` keeps symlink resolution beneath that handle, so swapping a path
//! component cannot redirect an operation to an ambient host path.

use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use cap_std::ambient_authority;
#[cfg(unix)]
use cap_std::fs::OpenOptionsExt;
use cap_std::fs::{Dir, File, OpenOptions};

const FORBIDDEN_SOURCE_EXTENSIONS: &[&str] =
    &["lua", "sh", "bash", "zsh", "fish", "py", "js", "ts", "rs"];

pub(crate) fn bounded_env_usize(name: &str, default: usize, hard_max: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .map(|value| value.min(hard_max))
        .unwrap_or(default.min(hard_max))
}

pub(crate) fn validate_relative(path: &Path) -> std::io::Result<()> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir
                    | Component::RootDir
                    | Component::Prefix(_)
                    | Component::CurDir
            )
        })
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "path must be a non-empty project-relative path without traversal",
        ));
    }
    Ok(())
}

/// Deny common credential, state, and VCS paths from every agent-facing read
/// primitive. This is a hard floor: prompt-injected agents must not bypass the
/// env allowlist by reading the flow's `.env` or persisted run state directly.
pub(crate) fn validate_agent_read_path(path: &Path) -> std::io::Result<()> {
    validate_relative(path)?;
    for component in path.components() {
        let Component::Normal(component) = component else {
            continue;
        };
        let name = component.to_string_lossy().to_ascii_lowercase();
        let sensitive_name = name == ".env"
            || name.starts_with(".env.")
            || matches!(
                name.as_str(),
                ".ironcrew"
                    | ".git"
                    | ".ssh"
                    | ".aws"
                    | ".azure"
                    | ".kube"
                    | ".docker"
                    | ".npmrc"
                    | ".pypirc"
                    | ".netrc"
                    | "credentials"
                    | "credentials.json"
                    | "secrets"
                    | "secrets.json"
                    | "id_rsa"
                    | "id_ed25519"
            )
            || name.starts_with("credentials.")
            || name.starts_with("secrets.")
            || ["pem", "key", "p12", "pfx", "jks", "kdbx"]
                .iter()
                .any(|extension| name.ends_with(&format!(".{extension}")));
        if sensitive_name {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "agent access to sensitive credential/state paths is denied",
            ));
        }
    }
    Ok(())
}

/// Reject flow source, executable, extensionless, and hidden control paths for
/// every agent-facing write primitive. Individual tools may apply a narrower
/// data-extension allowlist after this shared hard floor.
pub(crate) fn validate_agent_write_path(path: &Path) -> std::io::Result<()> {
    validate_relative(path)?;
    let extension = path.extension().and_then(|extension| extension.to_str());
    let Some(extension) = extension else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "extensionless and hidden control files are not writable",
        ));
    };
    if FORBIDDEN_SOURCE_EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str()) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("executable/source extension '.{extension}' is not writable"),
        ));
    }
    Ok(())
}

pub(crate) fn open_root(base_dir: Option<&Path>) -> std::io::Result<Dir> {
    Dir::open_ambient_dir(
        base_dir.unwrap_or_else(|| Path::new(".")),
        ambient_authority(),
    )
}

fn read_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.read(true);
    // Opening a FIFO for reading would otherwise wait indefinitely before we
    // could inspect its type. Non-blocking open lets us reject it below.
    options._cap_fs_ext_nonblock(true);
    #[cfg(unix)]
    options.custom_flags(nix::libc::O_NOFOLLOW);
    options
}

fn symlink_read_error() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        "agent reads do not follow symbolic links",
    )
}

/// Open a project-relative regular file without following a symlink in any
/// path component.
///
/// Each parent is opened relative to the preceding directory handle. On Unix,
/// `O_NOFOLLOW` makes the metadata-check/open sequence safe against a symlink
/// swap; `O_DIRECTORY` also guarantees that only a directory handle can become
/// the capability for the next component.
fn open_read_file_no_symlinks(root: &Dir, path: &Path) -> std::io::Result<File> {
    validate_relative(path)?;
    let components: Vec<_> = path.components().collect();
    let (file_component, parent_components) = components.split_last().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no file name")
    })?;
    let Component::Normal(file_name) = file_component else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "path contains an unsupported component",
        ));
    };

    let mut directory = root.try_clone()?;
    for component in parent_components {
        let Component::Normal(name) = component else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "path contains an unsupported component",
            ));
        };
        let metadata = directory.symlink_metadata(name)?;
        if metadata.file_type().is_symlink() {
            return Err(symlink_read_error());
        }
        if !metadata.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "a parent path component is not a directory",
            ));
        }

        #[cfg(unix)]
        {
            let mut options = OpenOptions::new();
            options.read(true).custom_flags(
                nix::libc::O_NOFOLLOW | nix::libc::O_DIRECTORY | nix::libc::O_NONBLOCK,
            );
            let opened = directory.open_with(name, &options)?;
            if !opened.metadata()?.is_dir() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "a parent path component is not a directory",
                ));
            }
            directory = Dir::from_std_file(opened.into_std());
        }
        #[cfg(not(unix))]
        {
            // cap-std still confines this open beneath `directory`; the
            // metadata checks reject stable symlinks on platforms without
            // Unix's O_NOFOLLOW.
            directory = directory.open_dir(name)?;
            if !directory.dir_metadata()?.is_dir() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "a parent path component is not a directory",
                ));
            }
        }
    }

    let metadata = directory.symlink_metadata(file_name)?;
    if metadata.file_type().is_symlink() {
        return Err(symlink_read_error());
    }

    let options = read_options();
    let file = directory.open_with(file_name, &options)?;
    if file.metadata()?.file_type().is_symlink() {
        return Err(symlink_read_error());
    }
    Ok(file)
}

pub(crate) fn read_utf8_bounded(
    root: &Dir,
    path: &Path,
    max_bytes: usize,
) -> std::io::Result<String> {
    let bytes = read_bytes_bounded(root, path, max_bytes)?;
    String::from_utf8(bytes).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("file is not valid UTF-8: {error}"),
        )
    })
}

pub(crate) fn read_bytes_bounded(
    root: &Dir,
    path: &Path,
    max_bytes: usize,
) -> std::io::Result<Vec<u8>> {
    let mut file = open_read_file_no_symlinks(root, path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "target is not a regular file",
        ));
    }
    if metadata.len() > max_bytes as u64 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::FileTooLarge,
            format!("file is {} bytes, limit is {max_bytes}", metadata.len()),
        ));
    }

    let mut bytes = Vec::with_capacity((metadata.len() as usize).min(max_bytes));
    std::io::Read::by_ref(&mut file)
        .take(max_bytes as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > max_bytes {
        return Err(std::io::Error::new(
            std::io::ErrorKind::FileTooLarge,
            format!("file grew beyond the {max_bytes}-byte limit while reading"),
        ));
    }
    Ok(bytes)
}

#[cfg(unix)]
fn sync_directory(directory: &Dir) -> std::io::Result<()> {
    // cap-std deliberately opens directory capabilities with O_PATH on Linux.
    // O_PATH descriptors are valid for openat-style confinement but fsync(2)
    // rejects them with EBADF. Reopen `.` beneath the capability as a real
    // read-only directory descriptor before flushing the rename.
    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(nix::libc::O_DIRECTORY | nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC);
    directory.open_with(".", &options)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(directory: &Dir) -> std::io::Result<()> {
    directory.try_clone()?.into_std_file().sync_all()
}

pub(crate) fn atomic_write(root: &Dir, path: &Path, content: &[u8]) -> std::io::Result<()> {
    validate_relative(path)?;
    let parent_path = path.parent().unwrap_or_else(|| Path::new(""));
    root.create_dir_all(parent_path)?;
    let parent = if parent_path.as_os_str().is_empty() {
        root.try_clone()?
    } else {
        root.open_dir(parent_path)?
    };
    let file_name = path.file_name().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no file name")
    })?;

    match parent.symlink_metadata(file_name) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "refusing to replace a symbolic link",
            ));
        }
        Ok(metadata) if !metadata.is_file() => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "target is not a regular file",
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    let temporary = PathBuf::from(format!(".ironcrew-write-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> std::io::Result<()> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = parent.open_with(&temporary, &options)?;
        file.write_all(content)?;
        file.sync_all()?;
        drop(file);
        parent.rename(&temporary, &parent, file_name)?;
        sync_directory(&parent)?;
        Ok(())
    })();

    if result.is_err() {
        let _ = parent.remove_file(&temporary);
    }
    result
}

pub(crate) fn collect_regular_files(
    root: &Dir,
    max_entries: usize,
) -> std::io::Result<(Vec<PathBuf>, bool)> {
    let mut pending = vec![(root.try_clone()?, PathBuf::new())];
    let mut files = Vec::new();
    let mut scanned = 0usize;

    while let Some((directory, prefix)) = pending.pop() {
        for entry in directory.entries()? {
            let entry = entry?;
            scanned += 1;
            if scanned > max_entries {
                return Ok((files, true));
            }

            let name = entry.file_name();
            let relative = prefix.join(&name);
            if validate_agent_read_path(&relative).is_err() {
                continue;
            }
            let file_type = entry.file_type()?;
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                if let Ok(child) = entry.open_dir() {
                    pending.push((child, relative));
                }
            } else if file_type.is_file() {
                files.push(relative);
            }
        }
    }
    Ok((files, false))
}
