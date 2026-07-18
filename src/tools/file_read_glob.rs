use async_trait::async_trait;
use serde_json::json;
use std::path::{Path, PathBuf};

use super::{Tool, ToolCallContext};
use crate::llm::provider::ToolSchema;
use crate::utils::error::{IronCrewError, Result};

const DEFAULT_GLOB_MAX_FILES: usize = 500;
const HARD_GLOB_MAX_FILES: usize = 10_000;
const DEFAULT_GLOB_MAX_BYTES: usize = 50 * 1024 * 1024;
const HARD_GLOB_MAX_BYTES: usize = 256 * 1024 * 1024;
const DEFAULT_GLOB_MAX_ENTRIES: usize = 10_000;
const HARD_GLOB_MAX_ENTRIES: usize = 100_000;
const DEFAULT_FILE_READ_MAX_BYTES: usize = 10 * 1024 * 1024;
const HARD_FILE_READ_MAX_BYTES: usize = 256 * 1024 * 1024;
const DEFAULT_GLOB_MAX_OUTPUT_BYTES: usize = 64 * 1024 * 1024;
const HARD_GLOB_MAX_OUTPUT_BYTES: usize = 256 * 1024 * 1024;
const HARD_GLOB_PATTERN_BYTES: usize = 8 * 1024;

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
        "Read multiple project-relative regular files matching a glob pattern. Returns a JSON object: {files: [{path, content}, ...], file_count, total_bytes, truncated}. Per-call limits: IRONCREW_GLOB_MAX_FILES (default 500), IRONCREW_GLOB_MAX_BYTES (default 50 MB), IRONCREW_GLOB_MAX_ENTRIES (default 10000), and IRONCREW_FILE_READ_MAX_BYTES (default 10 MB per file)."
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

    async fn execute(&self, args: serde_json::Value, _ctx: &ToolCallContext) -> Result<String> {
        let pattern = args["pattern"]
            .as_str()
            .ok_or_else(|| IronCrewError::ToolExecution {
                tool: "file_read_glob".into(),
                message: "Missing 'pattern' argument".into(),
            })?;
        if pattern.len() > HARD_GLOB_PATTERN_BYTES {
            return Err(IronCrewError::ToolExecution {
                tool: "file_read_glob".into(),
                message: format!(
                    "Glob pattern exceeds the {HARD_GLOB_PATTERN_BYTES}-byte hard limit"
                ),
            });
        }

        super::project_fs::validate_relative(Path::new(pattern)).map_err(|error| {
            IronCrewError::ToolExecution {
                tool: "file_read_glob".into(),
                message: format!("Pattern must be project-relative: {error}"),
            }
        })?;
        let matcher =
            glob::Pattern::new(pattern).map_err(|error| IronCrewError::ToolExecution {
                tool: "file_read_glob".into(),
                message: format!("Invalid glob pattern: {error}"),
            })?;

        // Resource budgets. Zero/invalid values fall back to bounded defaults.
        let max_files = super::project_fs::bounded_env_usize(
            "IRONCREW_GLOB_MAX_FILES",
            DEFAULT_GLOB_MAX_FILES,
            HARD_GLOB_MAX_FILES,
        );
        let max_total_bytes = super::project_fs::bounded_env_usize(
            "IRONCREW_GLOB_MAX_BYTES",
            DEFAULT_GLOB_MAX_BYTES,
            HARD_GLOB_MAX_BYTES,
        );
        let max_entries = super::project_fs::bounded_env_usize(
            "IRONCREW_GLOB_MAX_ENTRIES",
            DEFAULT_GLOB_MAX_ENTRIES,
            HARD_GLOB_MAX_ENTRIES,
        );
        let max_file_bytes = super::project_fs::bounded_env_usize(
            "IRONCREW_FILE_READ_MAX_BYTES",
            DEFAULT_FILE_READ_MAX_BYTES,
            HARD_FILE_READ_MAX_BYTES,
        );
        let base_dir = self.base_dir.clone();

        let (files, total_bytes, truncated) = tokio::task::spawn_blocking(move || {
            let root = super::project_fs::open_root(base_dir.as_deref())?;
            let (candidates, scan_truncated) =
                super::project_fs::collect_regular_files(&root, max_entries)?;
            let options = glob::MatchOptions {
                case_sensitive: true,
                require_literal_separator: true,
                require_literal_leading_dot: true,
            };
            let mut candidates: Vec<PathBuf> = candidates
                .into_iter()
                .filter(|path| matcher.matches_path_with(path, options))
                .collect();
            candidates.sort();

            let mut truncated = scan_truncated || candidates.len() > max_files;
            candidates.truncate(max_files);
            let mut files = Vec::with_capacity(candidates.len());
            let mut total_bytes = 0usize;

            for path in candidates {
                let display = path.display().to_string();
                let metadata = match root.metadata(&path) {
                    Ok(metadata) if metadata.is_file() => metadata,
                    Ok(_) => {
                        files.push(json!({
                            "path": display,
                            "error": "Refused non-regular file"
                        }));
                        continue;
                    }
                    Err(error) => {
                        files.push(json!({
                            "path": display,
                            "error": format!("Failed to inspect: {error}")
                        }));
                        continue;
                    }
                };
                if metadata.len() > max_file_bytes as u64 {
                    files.push(json!({
                        "path": display,
                        "error": format!(
                            "File is {} bytes, exceeds IRONCREW_FILE_READ_MAX_BYTES ({max_file_bytes})",
                            metadata.len()
                        )
                    }));
                    continue;
                }
                let remaining = max_total_bytes.saturating_sub(total_bytes);
                if metadata.len() > remaining as u64 || remaining == 0 {
                    truncated = true;
                    break;
                }

                match super::project_fs::read_utf8_bounded(
                    &root,
                    &path,
                    max_file_bytes.min(remaining),
                ) {
                    Ok(content) => {
                        total_bytes += content.len();
                        files.push(json!({ "path": display, "content": content }));
                    }
                    Err(error) => files.push(json!({
                        "path": display,
                        "error": format!("Failed to read: {error}")
                    })),
                }
            }
            Ok::<_, std::io::Error>((files, total_bytes, truncated))
        })
        .await
        .map_err(|error| IronCrewError::ToolExecution {
            tool: "file_read_glob".into(),
            message: format!("Filesystem worker failed: {error}"),
        })?
        .map_err(|error| IronCrewError::ToolExecution {
            tool: "file_read_glob".into(),
            message: format!("Glob read failed: {error}"),
        })?;

        let file_count = files.len();
        let output = json!({
            "files": files,
            "file_count": file_count,
            "total_bytes": total_bytes,
            "truncated": truncated,
        });

        let max_output_bytes = super::project_fs::bounded_env_usize(
            "IRONCREW_GLOB_MAX_OUTPUT_BYTES",
            DEFAULT_GLOB_MAX_OUTPUT_BYTES,
            HARD_GLOB_MAX_OUTPUT_BYTES,
        );
        crate::utils::http::to_json_pretty_limited(&output, max_output_bytes).map_err(|e| {
            IronCrewError::ToolExecution {
                tool: "file_read_glob".into(),
                message: format!("Serialization error: {}", e),
            }
        })
    }
}
