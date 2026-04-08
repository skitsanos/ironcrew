use std::path::{Path, PathBuf};

use crate::utils::error::{IronCrewError, Result};

pub struct ProjectLoader {
    project_dir: PathBuf,
    agent_files: Vec<PathBuf>,
    tool_files: Vec<PathBuf>,
    entrypoint: Option<PathBuf>,
}

impl ProjectLoader {
    pub fn from_directory(path: &Path) -> Result<Self> {
        let project_dir = path.to_path_buf();

        let entrypoint = project_dir.join("crew.lua");
        if !entrypoint.exists() {
            return Err(IronCrewError::Validation(format!(
                "No crew.lua found in {}",
                project_dir.display()
            )));
        }

        let agent_files = Self::discover_lua_files(&project_dir.join("agents"));
        let tool_files = Self::discover_lua_files(&project_dir.join("tools"));

        Ok(Self {
            project_dir,
            agent_files,
            tool_files,
            entrypoint: Some(entrypoint),
        })
    }

    pub fn from_file(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Err(IronCrewError::Validation(format!(
                "File not found: {}",
                path.display()
            )));
        }

        let project_dir = path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();

        let agent_files = Self::discover_lua_files(&project_dir.join("agents"));
        let tool_files = Self::discover_lua_files(&project_dir.join("tools"));

        Ok(Self {
            project_dir,
            agent_files,
            tool_files,
            entrypoint: Some(path.to_path_buf()),
        })
    }

    fn discover_lua_files(dir: &Path) -> Vec<PathBuf> {
        if !dir.is_dir() {
            return Vec::new();
        }

        let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("lua"))
            .collect();

        files.sort();
        files
    }

    pub fn project_dir(&self) -> &Path {
        &self.project_dir
    }

    pub fn agent_files(&self) -> &[PathBuf] {
        &self.agent_files
    }

    pub fn tool_files(&self) -> &[PathBuf] {
        &self.tool_files
    }

    pub fn entrypoint(&self) -> Option<&Path> {
        self.entrypoint.as_deref()
    }

    /// Path to `<project_dir>/config.lua` if it exists, otherwise `None`.
    /// config.lua is an optional file that returns a table of default settings
    /// merged into Crew.new() at runtime.
    pub fn config_lua_path(&self) -> Option<PathBuf> {
        let p = self.project_dir.join("config.lua");
        if p.is_file() { Some(p) } else { None }
    }
}
