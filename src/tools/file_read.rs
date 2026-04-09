use async_trait::async_trait;
use serde_json::json;
use std::path::{Component, Path, PathBuf};

use super::Tool;
use crate::llm::provider::ToolSchema;
use crate::utils::error::{IronCrewError, Result};

pub struct FileReadTool {
    base_dir: Option<PathBuf>,
}

impl FileReadTool {
    pub fn new(base_dir: Option<PathBuf>) -> Self {
        Self { base_dir }
    }

    fn validate_path(&self, path: &str) -> Result<PathBuf> {
        let path = Path::new(path);

        // Prevent absolute paths and directory traversal
        if path.is_absolute()
            || path.components().any(|c| {
                matches!(
                    c,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
        {
            return Err(IronCrewError::ToolExecution {
                tool: "file_read".into(),
                message: "Directory traversal not allowed".into(),
            });
        }

        if let Some(ref base) = self.base_dir {
            Ok(base.join(path))
        } else {
            Ok(path.to_path_buf())
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

    async fn execute(&self, args: serde_json::Value) -> Result<String> {
        let path = args["path"]
            .as_str()
            .ok_or_else(|| IronCrewError::ToolExecution {
                tool: "file_read".into(),
                message: "Missing 'path' argument".into(),
            })?;

        let validated = self.validate_path(path)?;

        // Check file size BEFORE reading the whole file into memory.
        let max_bytes: u64 = std::env::var("IRONCREW_FILE_READ_MAX_BYTES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(10 * 1024 * 1024); // 10 MB default

        let metadata =
            tokio::fs::metadata(&validated)
                .await
                .map_err(|e| IronCrewError::ToolExecution {
                    tool: "file_read".into(),
                    message: format!("Failed to stat '{}': {}", path, e),
                })?;

        if metadata.len() > max_bytes {
            return Err(IronCrewError::ToolExecution {
                tool: "file_read".into(),
                message: format!(
                    "File '{}' is {} bytes, exceeds limit of {} bytes (set IRONCREW_FILE_READ_MAX_BYTES to override)",
                    path,
                    metadata.len(),
                    max_bytes
                ),
            });
        }

        tokio::fs::read_to_string(&validated)
            .await
            .map_err(|e| IronCrewError::ToolExecution {
                tool: "file_read".into(),
                message: format!("Failed to read '{}': {}", path, e),
            })
    }
}
