use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

#[cfg(unix)]
use super::super::FramedDigest;
use super::super::validation;
use crate::utils::error::Result;

#[cfg(unix)]
const SOURCE_DOMAIN: &[u8] = b"ironcrew:conversation-flow-source:v1";

/// One immutable Lua file captured from a flow tree.
#[derive(Clone, Debug)]
pub struct SnapshotLuaSource {
    relative_path: PathBuf,
    source: Arc<str>,
}

impl SnapshotLuaSource {
    pub fn relative_path(&self) -> &Path {
        &self.relative_path
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn shared_source(&self) -> Arc<str> {
        self.source.clone()
    }
}

/// Loader inputs selected from one snapshot instead of live directory walks.
pub struct FlowSourceRoles {
    pub entrypoint: SnapshotLuaSource,
    pub config: Option<SnapshotLuaSource>,
    pub agents: Vec<SnapshotLuaSource>,
    pub tools: Vec<SnapshotLuaSource>,
}

/// A bounded, UTF-8, symlink-free observation of every Lua file in a flow.
#[derive(Debug)]
pub struct FlowSourceSnapshot {
    root: PathBuf,
    files: BTreeMap<PathBuf, Arc<str>>,
    fingerprint: String,
}

impl FlowSourceSnapshot {
    #[cfg(unix)]
    pub(super) fn from_files(root: PathBuf, files: BTreeMap<PathBuf, Arc<str>>) -> Result<Self> {
        let mut digest = FramedDigest::new(SOURCE_DOMAIN);
        digest.field(b"file_count", &(files.len() as u64).to_be_bytes());
        for (path, source) in &files {
            let canonical = canonical_relative_path(path)?;
            digest.field(b"path", canonical.as_bytes());
            digest.field(b"file", source.as_bytes());
        }
        Ok(Self {
            root,
            files,
            fingerprint: digest.finish(),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// Return an exact captured source. The path must already be lexical and
    /// relative; normalization that could hide traversal is intentionally absent.
    pub fn source(&self, relative_path: &Path) -> Result<Option<SnapshotLuaSource>> {
        validate_relative_path(relative_path, false)?;
        if relative_path.extension().and_then(|value| value.to_str()) != Some("lua") {
            return Err(validation("Lua source path must identify a .lua file"));
        }
        Ok(self
            .files
            .get(relative_path)
            .map(|source| SnapshotLuaSource {
                relative_path: relative_path.to_path_buf(),
                source: source.clone(),
            }))
    }

    pub fn roles(&self) -> Result<FlowSourceRoles> {
        let entrypoint = self.source(Path::new("crew.lua"))?.ok_or_else(|| {
            validation(format!(
                "No crew.lua found in immutable flow source {}",
                self.root.display()
            ))
        })?;
        Ok(FlowSourceRoles {
            entrypoint,
            config: self.source(Path::new("config.lua"))?,
            agents: self.direct_children(Path::new("agents"))?,
            tools: self.direct_children(Path::new("tools"))?,
        })
    }

    pub fn direct_children(&self, directory: &Path) -> Result<Vec<SnapshotLuaSource>> {
        validate_relative_path(directory, true)?;
        let mut sources = Vec::new();
        for (path, source) in &self.files {
            if path.parent() == Some(directory)
                && path.extension().and_then(|value| value.to_str()) == Some("lua")
            {
                sources.push(SnapshotLuaSource {
                    relative_path: path.clone(),
                    source: source.clone(),
                });
            }
        }
        Ok(sources)
    }

    /// Stem + source for every captured `sql/*.sql` file that is a *direct*
    /// child of `sql/`, in path order. Flat by design, matching the
    /// non-recursive filesystem discovery in `read_sql_dir` and the "one
    /// file per operation under sql/" docs contract — nested files like
    /// `sql/sub/x.sql` do not qualify (though they still participate in the
    /// recursive fingerprint capture in `unix.rs`).
    // only called from the postgres-gated wiring
    #[cfg_attr(not(feature = "postgres"), allow(dead_code))]
    pub fn sql_sources(&self) -> Vec<(String, Arc<str>)> {
        self.files
            .iter()
            .filter(|(path, _)| {
                path.parent() == Some(Path::new("sql"))
                    && path.extension().and_then(|e| e.to_str()) == Some("sql")
            })
            .filter_map(|(path, source)| {
                path.file_stem()
                    .and_then(|stem| stem.to_str())
                    .map(|stem| (stem.to_string(), source.clone()))
            })
            .collect()
    }
}

/// Snapshot plus the lexical directory used by `require` and `run_flow`.
#[derive(Clone, Debug)]
pub struct ConversationSourceContext {
    pub snapshot: Arc<FlowSourceSnapshot>,
    logical_dir: PathBuf,
}

impl ConversationSourceContext {
    pub fn root(snapshot: Arc<FlowSourceSnapshot>) -> Self {
        Self {
            snapshot,
            logical_dir: PathBuf::new(),
        }
    }

    pub fn with_logical_dir(
        snapshot: Arc<FlowSourceSnapshot>,
        logical_dir: PathBuf,
    ) -> Result<Self> {
        validate_relative_path(&logical_dir, true)?;
        Ok(Self {
            snapshot,
            logical_dir,
        })
    }

    pub fn logical_dir(&self) -> &Path {
        &self.logical_dir
    }

    pub fn logical_path(&self, path: &str) -> Result<PathBuf> {
        let path = Path::new(path);
        validate_relative_path(path, false)?;
        Ok(self.logical_dir.join(path))
    }

    pub fn source(&self, path: &str) -> Result<Option<SnapshotLuaSource>> {
        self.snapshot.source(&self.logical_path(path)?)
    }

    pub fn child_for_source(&self, source: &SnapshotLuaSource) -> Result<Self> {
        let parent = source
            .relative_path()
            .parent()
            .unwrap_or_else(|| Path::new(""));
        Self::with_logical_dir(self.snapshot.clone(), parent.to_path_buf())
    }

    pub fn direct_children(&self, directory: &str) -> Result<Vec<SnapshotLuaSource>> {
        let directory = self.logical_path(directory)?;
        self.snapshot.direct_children(&directory)
    }
}

fn validate_relative_path(path: &Path, allow_empty: bool) -> Result<()> {
    if path.as_os_str().is_empty() {
        return if allow_empty {
            Ok(())
        } else {
            Err(validation("Lua source path must not be empty"))
        };
    }
    if path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(validation(
            "Lua source path must be a canonical relative path",
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn canonical_relative_path(path: &Path) -> Result<String> {
    validate_relative_path(path, false)?;
    path.components()
        .map(|component| match component {
            Component::Normal(part) => part
                .to_str()
                .map(str::to_owned)
                .ok_or_else(|| validation("flow source relative path must be valid UTF-8")),
            _ => unreachable!("validated relative components"),
        })
        .collect::<Result<Vec<_>>>()
        .map(|parts| parts.join("/"))
}
