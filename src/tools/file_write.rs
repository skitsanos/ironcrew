use async_trait::async_trait;
use serde_json::json;
use std::path::{Path, PathBuf};

use super::{Tool, ToolCallContext};
use crate::llm::provider::ToolSchema;
use crate::utils::error::{IronCrewError, Result};

const DEFAULT_FILE_WRITE_MAX_BYTES: usize = 10 * 1024 * 1024;
const HARD_FILE_WRITE_MAX_BYTES: usize = 256 * 1024 * 1024;
pub struct FileWriteTool {
    base_dir: Option<PathBuf>,
    allowed_extensions: Vec<String>,
}

impl FileWriteTool {
    pub fn new(base_dir: Option<PathBuf>, allowed_extensions: Option<Vec<String>>) -> Self {
        Self {
            base_dir,
            allowed_extensions: allowed_extensions.unwrap_or_else(|| {
                vec![
                    "txt", "md", "json", "csv", "yaml", "yml", "toml", "xml", "html", "css",
                ]
                .into_iter()
                .map(String::from)
                .collect()
            }),
        }
    }

    fn validate_path(&self, path: &str) -> Result<()> {
        let path = Path::new(path);
        super::project_fs::validate_agent_write_path(path).map_err(|error| {
            IronCrewError::ToolExecution {
                tool: "file_write".into(),
                message: format!("Write path is not allowed: {error}"),
            }
        })?;

        let extension = path.extension().and_then(|extension| extension.to_str());
        let extension = extension.expect("shared validation requires an extension");
        if !self
            .allowed_extensions
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(extension))
        {
            return Err(IronCrewError::ToolExecution {
                tool: "file_write".into(),
                message: format!("Extension '.{extension}' not allowed"),
            });
        }

        Ok(())
    }
}

#[async_trait]
impl Tool for FileWriteTool {
    fn name(&self) -> &str {
        "file_write"
    }

    fn description(&self) -> &str {
        "Write content to a file"
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "file_write".into(),
            description: "Write content to a file".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file to write"
                    },
                    "content": {
                        "type": "string",
                        "description": "Content to write to the file"
                    }
                },
                "required": ["path", "content"]
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value, _ctx: &ToolCallContext) -> Result<String> {
        let path = args["path"]
            .as_str()
            .ok_or_else(|| IronCrewError::ToolExecution {
                tool: "file_write".into(),
                message: "Missing 'path' argument".into(),
            })?;
        let content = args["content"]
            .as_str()
            .ok_or_else(|| IronCrewError::ToolExecution {
                tool: "file_write".into(),
                message: "Missing 'content' argument".into(),
            })?;

        self.validate_path(path)?;

        let max_bytes = super::project_fs::bounded_env_usize(
            "IRONCREW_FILE_WRITE_MAX_BYTES",
            DEFAULT_FILE_WRITE_MAX_BYTES,
            HARD_FILE_WRITE_MAX_BYTES,
        );
        if content.len() > max_bytes {
            return Err(IronCrewError::ToolExecution {
                tool: "file_write".into(),
                message: format!(
                    "Content is {} bytes, exceeds IRONCREW_FILE_WRITE_MAX_BYTES ({max_bytes})",
                    content.len()
                ),
            });
        }

        let base_dir = self.base_dir.clone();
        let relative = PathBuf::from(path);
        let bytes = content.as_bytes().to_vec();
        let display = path.to_string();
        tokio::task::spawn_blocking(move || {
            let root = super::project_fs::open_root(base_dir.as_deref())?;
            super::project_fs::atomic_write(&root, &relative, &bytes)
        })
        .await
        .map_err(|error| IronCrewError::ToolExecution {
            tool: "file_write".into(),
            message: format!("Filesystem worker failed for '{display}': {error}"),
        })?
        .map_err(|error| IronCrewError::ToolExecution {
            tool: "file_write".into(),
            message: format!("Failed to write '{display}': {error}"),
        })?;

        Ok(format!(
            "Successfully wrote {} bytes to {}",
            content.len(),
            path
        ))
    }
}
