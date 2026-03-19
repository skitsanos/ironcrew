use async_trait::async_trait;
use serde_json::json;
use std::path::PathBuf;

use super::Tool;
use crate::llm::provider::ToolSchema;
use crate::utils::error::{IronCrewError, Result};

#[derive(Default)]
pub struct FileReadGlobTool {
    base_dir: Option<PathBuf>,
}

impl FileReadGlobTool {
    pub fn new(base_dir: Option<PathBuf>) -> Self {
        Self { base_dir }
    }
}

#[async_trait]
impl Tool for FileReadGlobTool {
    fn name(&self) -> &str {
        "file_read_glob"
    }

    fn description(&self) -> &str {
        "Read multiple files matching a glob pattern and return them as a JSON array of {path, content} objects"
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "file_read_glob".into(),
            description: self.description().into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Glob pattern relative to project directory (e.g., 'input/reports/*.md', 'data/**/*.json')"
                    }
                },
                "required": ["pattern"]
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value) -> Result<String> {
        let pattern = args["pattern"]
            .as_str()
            .ok_or_else(|| IronCrewError::ToolExecution {
                tool: "file_read_glob".into(),
                message: "Missing 'pattern' argument".into(),
            })?;

        // Validate the pattern doesn't escape
        if pattern.contains("..") || pattern.starts_with('/') {
            return Err(IronCrewError::ToolExecution {
                tool: "file_read_glob".into(),
                message: "Pattern must not contain '..' or start with '/'".into(),
            });
        }

        let full_pattern = if let Some(ref base) = self.base_dir {
            format!("{}/{}", base.display(), pattern)
        } else {
            pattern.to_string()
        };

        let mut results = Vec::new();

        let entries = glob::glob(&full_pattern).map_err(|e| IronCrewError::ToolExecution {
            tool: "file_read_glob".into(),
            message: format!("Invalid glob pattern: {}", e),
        })?;

        for entry in entries {
            let path = entry.map_err(|e| IronCrewError::ToolExecution {
                tool: "file_read_glob".into(),
                message: format!("Glob error: {}", e),
            })?;

            if path.is_file() {
                let relative = if let Some(ref base) = self.base_dir {
                    path.strip_prefix(base).unwrap_or(&path).to_path_buf()
                } else {
                    path.clone()
                };

                match tokio::fs::read_to_string(&path).await {
                    Ok(content) => {
                        results.push(json!({
                            "path": relative.display().to_string(),
                            "content": content
                        }));
                    }
                    Err(e) => {
                        results.push(json!({
                            "path": relative.display().to_string(),
                            "error": format!("Failed to read: {}", e)
                        }));
                    }
                }
            }
        }

        // Sort by path for deterministic output
        results.sort_by(|a, b| {
            a["path"]
                .as_str()
                .unwrap_or("")
                .cmp(b["path"].as_str().unwrap_or(""))
        });

        serde_json::to_string_pretty(&results).map_err(|e| IronCrewError::ToolExecution {
            tool: "file_read_glob".into(),
            message: format!("Serialization error: {}", e),
        })
    }
}
