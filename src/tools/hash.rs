use async_trait::async_trait;
use serde_json::json;
use std::fmt::Write;

use super::Tool;
use crate::llm::provider::ToolSchema;
use crate::utils::error::{IronCrewError, Result};

#[derive(Default)]
pub struct HashTool;

impl HashTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for HashTool {
    fn name(&self) -> &str {
        "hash"
    }
    fn description(&self) -> &str {
        "Compute a hash (MD5, SHA256, SHA512) of the input text"
    }
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "hash".into(),
            description: self.description().into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "text": { "type": "string", "description": "Text to hash" },
                    "algorithm": { "type": "string", "description": "Hash algorithm: md5, sha256, sha512", "enum": ["md5", "sha256", "sha512"] }
                },
                "required": ["text", "algorithm"]
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value) -> Result<String> {
        let text = args["text"]
            .as_str()
            .ok_or_else(|| IronCrewError::ToolExecution {
                tool: "hash".into(),
                message: "Missing 'text' argument".into(),
            })?;
        let algorithm = args["algorithm"].as_str().unwrap_or("sha256");

        use md5::Md5;
        use sha2::{Digest, Sha256, Sha512};

        let hash_hex = match algorithm {
            "md5" => {
                let mut hasher = Md5::new();
                hasher.update(text.as_bytes());
                let result = hasher.finalize();
                result.iter().fold(String::new(), |mut s, b| {
                    let _ = write!(s, "{:02x}", b);
                    s
                })
            }
            "sha256" => {
                let mut hasher = Sha256::new();
                hasher.update(text.as_bytes());
                let result = hasher.finalize();
                result.iter().fold(String::new(), |mut s, b| {
                    let _ = write!(s, "{:02x}", b);
                    s
                })
            }
            "sha512" => {
                let mut hasher = Sha512::new();
                hasher.update(text.as_bytes());
                let result = hasher.finalize();
                result.iter().fold(String::new(), |mut s, b| {
                    let _ = write!(s, "{:02x}", b);
                    s
                })
            }
            other => {
                return Err(IronCrewError::ToolExecution {
                    tool: "hash".into(),
                    message: format!("Unsupported algorithm: {}", other),
                });
            }
        };

        Ok(hash_hex)
    }
}
