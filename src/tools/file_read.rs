use async_trait::async_trait;
use serde_json::json;
use std::path::{Path, PathBuf};

use super::{Tool, ToolCallContext};
use crate::llm::provider::ToolSchema;
use crate::utils::error::{IronCrewError, Result};

const DEFAULT_FILE_READ_MAX_BYTES: usize = 10 * 1024 * 1024;
const HARD_FILE_READ_MAX_BYTES: usize = 256 * 1024 * 1024;

pub struct FileReadTool {
    root: super::execution_policy::CapabilityRoot,
    max_bytes: usize,
}

impl FileReadTool {
    pub fn new(base_dir: Option<PathBuf>) -> Self {
        Self {
            root: super::execution_policy::CapabilityRoot::required(base_dir),
            max_bytes: super::project_fs::bounded_env_usize(
                "IRONCREW_FILE_READ_MAX_BYTES",
                DEFAULT_FILE_READ_MAX_BYTES,
                HARD_FILE_READ_MAX_BYTES,
            ),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_max_bytes_for_test(base_dir: PathBuf, max_bytes: usize) -> Self {
        Self {
            root: super::execution_policy::CapabilityRoot::required(Some(base_dir)),
            max_bytes,
        }
    }
}

#[async_trait]
impl Tool for FileReadTool {
    fn name(&self) -> &str {
        "file_read"
    }

    fn description(&self) -> &str {
        "Read the contents of a file"
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "file_read".into(),
            description: "Read the contents of a file".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file to read"
                    }
                },
                "required": ["path"]
            }),
        }
    }

    fn conversation_definition(&self) -> Result<serde_json::Value> {
        Ok(json!({
            "schema": self.schema(),
            "root_fingerprint": self.root.fingerprint()?,
            "max_bytes": self.max_bytes,
        }))
    }

    async fn execute(&self, args: serde_json::Value, _ctx: &ToolCallContext) -> Result<String> {
        let path = args["path"]
            .as_str()
            .ok_or_else(|| IronCrewError::ToolExecution {
                tool: "file_read".into(),
                message: "Missing 'path' argument".into(),
            })?;

        let relative = Path::new(path);
        super::project_fs::validate_agent_read_path(relative).map_err(|error| {
            IronCrewError::ToolExecution {
                tool: "file_read".into(),
                message: format!("Read path is not allowed: {error}"),
            }
        })?;

        let max_bytes = self.max_bytes;
        let base_dir = self
            .root
            .cloned_path()
            .map_err(|message| IronCrewError::ToolExecution {
                tool: "file_read".into(),
                message: message.into(),
            })?;
        let relative = relative.to_path_buf();
        let display = path.to_string();
        tokio::task::spawn_blocking(move || {
            let root = super::project_fs::open_root(Some(&base_dir))?;
            super::project_fs::read_utf8_bounded(&root, &relative, max_bytes)
        })
        .await
        .map_err(|error| IronCrewError::ToolExecution {
            tool: "file_read".into(),
            message: format!("Filesystem worker failed for '{display}': {error}"),
        })?
        .map_err(|error| IronCrewError::ToolExecution {
            tool: "file_read".into(),
            message: format!("Failed to read '{display}': {error}"),
        })
    }
}
