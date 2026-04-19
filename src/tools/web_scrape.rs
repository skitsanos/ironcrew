use async_trait::async_trait;
use reqwest::Client;
use scraper::{Html, Selector};
use serde_json::json;
use std::time::Duration;

use super::{Tool, ToolCallContext};
use crate::llm::provider::ToolSchema;
use crate::utils::error::{IronCrewError, Result};

pub struct WebScrapeTool {
    client: Client,
    allowed_domains: Option<Vec<String>>,
}

impl WebScrapeTool {
    pub fn new(allowed_domains: Option<Vec<String>>) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent("IronCrew/0.1")
            .build()
            .expect("Failed to build HTTP client");

        Self {
            client,
            allowed_domains,
        }
    }

    fn is_domain_allowed(&self, url: &str) -> bool {
        let Some(ref domains) = self.allowed_domains else {
            return true;
        };

        let Ok(parsed) = url::Url::parse(url) else {
            return false;
        };

        let Some(host) = parsed.host_str() else {
            return false;
        };

        domains.iter().any(|d| {
            if d.starts_with("*.") {
                host.ends_with(&d[1..]) || host == &d[2..]
            } else {
                host == d
            }
        })
    }
}

#[async_trait]
impl Tool for WebScrapeTool {
    fn name(&self) -> &str {
        "web_scrape"
    }

    fn description(&self) -> &str {
        "Fetch a URL and extract its text content"
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "web_scrape".into(),
            description: "Fetch a URL and extract its text content".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "URL to scrape"
                    }
                },
                "required": ["url"]
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value, _ctx: &ToolCallContext) -> Result<String> {
        let url = args["url"]
            .as_str()
            .ok_or_else(|| IronCrewError::ToolExecution {
                tool: "web_scrape".into(),
                message: "Missing 'url' argument".into(),
            })?;

        if !self.is_domain_allowed(url) {
            return Err(IronCrewError::ToolExecution {
                tool: "web_scrape".into(),
                message: format!("Domain not in allowed list: {}", url),
            });
        }

        let resp = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| IronCrewError::ToolExecution {
                tool: "web_scrape".into(),
                message: format!("Failed to fetch '{}': {}", url, e),
            })?;

        // Cap HTML bytes BEFORE parsing into the DOM. Very large HTML
        // documents can cause quadratic parser behavior and consume
        // disproportionate RAM during DOM construction.
        let max_html_bytes: usize = std::env::var("IRONCREW_WEB_SCRAPE_MAX_BYTES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(2 * 1024 * 1024); // 2 MB default

        if let Some(len) = resp.content_length()
            && len as usize > max_html_bytes
        {
            return Err(IronCrewError::ToolExecution {
                tool: "web_scrape".into(),
                message: format!(
                    "HTML response too large: {} bytes (limit: {} bytes)",
                    len, max_html_bytes
                ),
            });
        }

        // Stream with byte cap (handles chunked responses with no header).
        use futures::StreamExt;
        let mut stream = resp.bytes_stream();
        let mut html_bytes: Vec<u8> = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| IronCrewError::ToolExecution {
                tool: "web_scrape".into(),
                message: format!("Failed to read response: {}", e),
            })?;
            if html_bytes.len() + chunk.len() > max_html_bytes {
                return Err(IronCrewError::ToolExecution {
                    tool: "web_scrape".into(),
                    message: format!(
                        "HTML response exceeded max size of {} bytes while streaming",
                        max_html_bytes
                    ),
                });
            }
            html_bytes.extend_from_slice(&chunk);
        }
        let html = String::from_utf8_lossy(&html_bytes).into_owned();

        let document = Html::parse_document(&html);
        let body_selector = Selector::parse("body").unwrap();

        let text = document
            .select(&body_selector)
            .flat_map(|el| el.text())
            .collect::<Vec<_>>()
            .join(" ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");

        // Truncate to avoid overwhelming LLM context (UTF-8 safe)
        let truncated = if text.chars().count() > 10000 {
            let s: String = text.chars().take(10000).collect();
            format!("{}... [truncated]", s)
        } else {
            text
        };

        Ok(truncated)
    }
}
