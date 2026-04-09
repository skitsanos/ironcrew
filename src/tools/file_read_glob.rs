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
        "Read multiple files matching a glob pattern. Returns a JSON object: {files: [{path, content}, ...], file_count, total_bytes, truncated}. Per-call limits: IRONCREW_GLOB_MAX_FILES (default 500), IRONCREW_GLOB_MAX_BYTES (default 50 MB)."
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

        // Resource budgets (see docs/cli.md). Set either to 0 to disable.
        let max_files: usize = std::env::var("IRONCREW_GLOB_MAX_FILES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(500);
        let max_total_bytes: u64 = std::env::var("IRONCREW_GLOB_MAX_BYTES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(50 * 1024 * 1024); // 50 MB

        let full_pattern = if let Some(ref base) = self.base_dir {
            format!("{}/{}", base.display(), pattern)
        } else {
            pattern.to_string()
        };

        let mut files = Vec::new();
        let mut total_bytes: u64 = 0;
        let mut truncated = false;

        let entries = glob::glob(&full_pattern).map_err(|e| IronCrewError::ToolExecution {
            tool: "file_read_glob".into(),
            message: format!("Invalid glob pattern: {}", e),
        })?;

        for entry in entries {
            // Stop if we've hit the file-count cap.
            if max_files > 0 && files.len() >= max_files {
                truncated = true;
                break;
            }

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
                        // Enforce total-byte budget BEFORE appending, so we never
                        // materialize a read that would push us over the cap.
                        let next_total = total_bytes + content.len() as u64;
                        if max_total_bytes > 0 && next_total > max_total_bytes {
                            truncated = true;
                            break;
                        }
                        total_bytes = next_total;
                        files.push(json!({
                            "path": relative.display().to_string(),
                            "content": content
                        }));
                    }
                    Err(e) => {
                        files.push(json!({
                            "path": relative.display().to_string(),
                            "error": format!("Failed to read: {}", e)
                        }));
                    }
                }
            }
        }

        // Sort by path for deterministic output
        files.sort_by(|a, b| {
            a["path"]
                .as_str()
                .unwrap_or("")
                .cmp(b["path"].as_str().unwrap_or(""))
        });

        let file_count = files.len();
        let output = json!({
            "files": files,
            "file_count": file_count,
            "total_bytes": total_bytes,
            "truncated": truncated,
        });

        serde_json::to_string_pretty(&output).map_err(|e| IronCrewError::ToolExecution {
            tool: "file_read_glob".into(),
            message: format!("Serialization error: {}", e),
        })
    }
}
