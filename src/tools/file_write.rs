use async_trait::async_trait;
use serde_json::json;
use std::path::{Path, PathBuf};

use super::Tool;
use crate::llm::provider::ToolSchema;
use crate::utils::error::{IronCrewError, Result};

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
                    "txt", "md", "json", "csv", "yaml", "yml", "toml", "xml",
                    "html", "css", "js", "ts", "py", "rs", "lua", "sh",
                ]
                .into_iter()
                .map(String::from)
                .collect()
            }),
        }
    }

    fn validate_path(&self, path: &str) -> Result<PathBuf> {
        let path = Path::new(path);

        if path.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
            return Err(IronCrewError::ToolExecution {
                tool: "file_write".into(),
                message: "Directory traversal not allowed".into(),
            });
        }

        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            if !self.allowed_extensions.iter().any(|a| a == ext) {
                return Err(IronCrewError::ToolExecution {
                    tool: "file_write".into(),
                    message: format!("Extension '.{}' not allowed", ext),
                });
            }
        }

        if let Some(ref base) = self.base_dir {
            Ok(base.join(path))
        } else {
            Ok(path.to_path_buf())
        }
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

    async fn execute(&self, args: serde_json::Value) -> Result<String> {
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

        let validated = self.validate_path(path)?;

        if let Some(parent) = validated.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| {
                IronCrewError::ToolExecution {
                    tool: "file_write".into(),
                    message: format!("Failed to create directories: {}", e),
                }
            })?;
        }

        tokio::fs::write(&validated, content)
            .await
            .map_err(|e| IronCrewError::ToolExecution {
                tool: "file_write".into(),
                message: format!("Failed to write '{}': {}", path, e),
            })?;

        Ok(format!("Successfully wrote {} bytes to {}", content.len(), path))
    }
}
