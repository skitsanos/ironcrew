use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cap_std::fs::{Dir, Metadata, OpenOptions};

use super::super::validation;
use super::snapshot::FlowSourceSnapshot;
use super::support::{
    changed_path, configured_file_limit, invalid_lua_path, same_object, same_snapshot,
    source_too_large,
};
use crate::utils::error::{IronCrewError, Result};

const MAX_AGGREGATE_BYTES: usize = 64 * 1024 * 1024;
const MAX_LUA_FILES: usize = 1_024;
const MAX_TREE_ENTRIES: usize = 16_384;
const MAX_TREE_DEPTH: usize = 32;

pub fn capture_flow_source(flow_root: &Path) -> Result<FlowSourceSnapshot> {
    let root = open_root(flow_root)?;
    let mut capture = Capture {
        display_root: flow_root,
        file_limit: configured_file_limit()?,
        entries: 0,
        aggregate_bytes: 0,
        files: BTreeMap::new(),
    };
    capture.collect(&root, &PathBuf::new(), 0)?;
    FlowSourceSnapshot::from_files(flow_root.to_path_buf(), capture.files)
}

struct Capture<'a> {
    display_root: &'a Path,
    file_limit: usize,
    entries: usize,
    aggregate_bytes: usize,
    files: BTreeMap<PathBuf, Arc<str>>,
}

impl Capture<'_> {
    fn collect(&mut self, directory: &Dir, prefix: &Path, depth: usize) -> Result<()> {
        for entry in directory.entries()? {
            let entry = entry?;
            self.entries = self.entries.checked_add(1).ok_or_else(tree_entry_limit)?;
            if self.entries > MAX_TREE_ENTRIES {
                return Err(tree_entry_limit());
            }

            let name = entry.file_name();
            let relative = prefix.join(&name);
            let display_path = self.display_root.join(&relative);
            let listed = directory.symlink_metadata(&name)?;
            let file_type = listed.file_type();
            if file_type.is_symlink() {
                return Err(validation(format!(
                    "flow tree must not contain symlinks: {}",
                    display_path.display()
                )));
            }

            let extension = relative.extension().and_then(|value| value.to_str());
            let is_source = matches!(extension, Some("lua") | Some("sql"));
            if is_source {
                self.capture_lua(directory, &name, relative, display_path, &listed)?;
            } else if file_type.is_dir() {
                if depth >= MAX_TREE_DEPTH {
                    return Err(validation(format!(
                        "flow tree exceeds the maximum depth of {MAX_TREE_DEPTH}"
                    )));
                }
                let child = open_directory(directory, &name, &display_path, &listed)?;
                self.collect(&child, &relative, depth + 1)?;
            }
        }
        Ok(())
    }

    fn capture_lua(
        &mut self,
        directory: &Dir,
        name: &std::ffi::OsStr,
        relative: PathBuf,
        display_path: PathBuf,
        listed: &Metadata,
    ) -> Result<()> {
        if !listed.is_file() {
            return Err(invalid_lua_path(&display_path));
        }
        if self.files.len() >= MAX_LUA_FILES {
            return Err(validation(format!(
                "flow tree exceeds the limit of {MAX_LUA_FILES} source files"
            )));
        }
        let bytes = read_lua_file(directory, name, &display_path, listed, self.file_limit)?;
        self.aggregate_bytes = self
            .aggregate_bytes
            .checked_add(bytes.len())
            .filter(|bytes| *bytes <= MAX_AGGREGATE_BYTES)
            .ok_or_else(|| {
                validation(format!(
                    "flow Lua sources exceed the aggregate limit of {MAX_AGGREGATE_BYTES} bytes"
                ))
            })?;
        let source = String::from_utf8(bytes).map_err(|error| {
            validation(format!(
                "Lua source '{}' is not valid UTF-8: {error}",
                display_path.display()
            ))
        })?;
        self.files.insert(relative, Arc::from(source));
        Ok(())
    }
}

fn open_root(path: &Path) -> Result<Dir> {
    let listed = fs::symlink_metadata(path)?;
    if listed.file_type().is_symlink() || !listed.is_dir() {
        return Err(validation(format!(
            "flow root must be a regular directory, not a symlink or special file: {}",
            path.display()
        )));
    }
    let mut options = fs::OpenOptions::new();
    options.read(true);
    std::os::unix::fs::OpenOptionsExt::custom_flags(
        &mut options,
        nix::libc::O_DIRECTORY
            | nix::libc::O_NOFOLLOW
            | nix::libc::O_NONBLOCK
            | nix::libc::O_CLOEXEC,
    );
    let opened = options.open(path)?;
    let metadata = opened.metadata()?;
    if !metadata.is_dir()
        || std::os::unix::fs::MetadataExt::dev(&listed)
            != std::os::unix::fs::MetadataExt::dev(&metadata)
        || std::os::unix::fs::MetadataExt::ino(&listed)
            != std::os::unix::fs::MetadataExt::ino(&metadata)
    {
        return Err(changed_path(path));
    }
    Ok(Dir::from_std_file(opened))
}

fn open_directory(
    parent: &Dir,
    name: &std::ffi::OsStr,
    display_path: &Path,
    listed: &Metadata,
) -> Result<Dir> {
    let mut options = OpenOptions::new();
    options.read(true);
    cap_std::fs::OpenOptionsExt::custom_flags(
        &mut options,
        nix::libc::O_DIRECTORY
            | nix::libc::O_NOFOLLOW
            | nix::libc::O_NONBLOCK
            | nix::libc::O_CLOEXEC,
    );
    let opened = parent.open_with(name, &options)?;
    let metadata = opened.metadata()?;
    if !metadata.is_dir() || !same_object(listed, &metadata) {
        return Err(changed_path(display_path));
    }
    Ok(Dir::from_std_file(opened.into_std()))
}

fn read_lua_file(
    parent: &Dir,
    name: &std::ffi::OsStr,
    display_path: &Path,
    listed: &Metadata,
    limit: usize,
) -> Result<Vec<u8>> {
    let mut options = OpenOptions::new();
    options.read(true);
    cap_std::fs::OpenOptionsExt::custom_flags(
        &mut options,
        nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK | nix::libc::O_CLOEXEC,
    );
    let mut file = parent.open_with(name, &options)?;
    let before = file.metadata()?;
    if !before.is_file() || !same_object(listed, &before) {
        return Err(changed_path(display_path));
    }
    if before.len() > limit as u64 {
        return Err(source_too_large(display_path, limit));
    }
    let mut bytes = Vec::with_capacity((before.len() as usize).min(limit));
    Read::by_ref(&mut file)
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(source_too_large(display_path, limit));
    }
    let after = file.metadata()?;
    if !same_snapshot(&before, &after) {
        return Err(changed_path(display_path));
    }
    Ok(bytes)
}

fn tree_entry_limit() -> IronCrewError {
    validation(format!(
        "flow tree exceeds the traversal limit of {MAX_TREE_ENTRIES} entries"
    ))
}
