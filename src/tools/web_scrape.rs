use async_trait::async_trait;
use reqwest::Client;
use scraper::{Html, Selector};
use serde_json::json;
use std::time::Duration;

use super::{Tool, ToolCallContext};
use crate::llm::provider::ToolSchema;
use crate::utils::error::{IronCrewError, Result};

mod policy;
use policy::WebScrapePolicy;

pub struct WebScrapeTool {
    client: Client,
    policy: WebScrapePolicy,
}

impl WebScrapeTool {
    pub fn new(allowed_domains: Option<Vec<String>>) -> Self {
        let policy = WebScrapePolicy::capture(allowed_domains);
        let redirect_domains = policy.allowed_domains().map(<[String]>::to_vec);
        let allow_private = policy.allow_private();
        let client = crate::utils::network::secure_client_builder_with_private_access(
            crate::utils::network::OutboundNetworkPolicy::PublicOnly,
            allow_private,
        )
        .redirect(reqwest::redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() >= 10 {
                return attempt.error("too many redirects (max 10)".to_string());
            }
            if let Err(reason) = crate::utils::network::validate_url_with_private_access(
                attempt.url().as_str(),
                crate::utils::network::OutboundNetworkPolicy::PublicOnly,
                allow_private,
            ) {
                return attempt.error(reason);
            }
            if !Self::is_domain_allowed_for(redirect_domains.as_deref(), attempt.url().as_str()) {
                return attempt.error("redirect target is not in the allowed domain list");
            }
            attempt.follow()
        }))
        .timeout(Duration::from_secs(30))
        .user_agent("IronCrew/0.1")
        .build()
        .expect("Failed to build HTTP client");

        Self { client, policy }
    }

    #[cfg(test)]
    pub(crate) fn with_policy_for_test(
        allowed_domains: Option<Vec<String>>,
        max_html_bytes: usize,
        allow_private: bool,
    ) -> Self {
        let policy = WebScrapePolicy::from_values(allowed_domains, max_html_bytes, allow_private);
        let client = if allow_private {
            super::http_request::PRIVATE_HTTP_CLIENT.clone()
        } else {
            super::http_request::PUBLIC_HTTP_CLIENT.clone()
        };
        Self { client, policy }
    }

    fn is_domain_allowed(&self, url: &str) -> bool {
        Self::is_domain_allowed_for(self.policy.allowed_domains(), url)
    }

    fn is_domain_allowed_for(domains: Option<&[String]>, url: &str) -> bool {
        let Some(domains) = domains else {
            return true;
        };

        let Ok(parsed) = url::Url::parse(url) else {
            return false;
        };

        let Some(host) = parsed.host_str() else {
            return false;
        };

        let host = host.to_ascii_lowercase();
        domains.iter().any(|domain| {
            let d = domain.to_ascii_lowercase();
            if let Some(apex) = d.strip_prefix("*.") {
                host == apex
                    || host
                        .strip_suffix(apex)
                        .is_some_and(|prefix| prefix.ends_with('.'))
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

    fn conversation_definition(&self) -> Result<serde_json::Value> {
        Ok(json!({
            "schema": self.schema(),
            "policy": self.policy.definition(),
        }))
    }

    async fn execute(&self, args: serde_json::Value, _ctx: &ToolCallContext) -> Result<String> {
        let url = args["url"]
            .as_str()
            .ok_or_else(|| IronCrewError::ToolExecution {
                tool: "web_scrape".into(),
                message: "Missing 'url' argument".into(),
            })?;

        // SSRF protection: block private/internal IPs (parity with http_request).
        crate::utils::network::validate_url_with_private_access(
            url,
            crate::utils::network::OutboundNetworkPolicy::PublicOnly,
            self.policy.allow_private(),
        )
        .map_err(|e| IronCrewError::ToolExecution {
            tool: "web_scrape".into(),
            message: e,
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
        let max_html_bytes = self.policy.max_html_bytes();
        let html_bytes =
            crate::utils::http::read_response_bytes(resp, max_html_bytes, "web scrape response")
                .await
                .map_err(|error| IronCrewError::ToolExecution {
                    tool: "web_scrape".into(),
                    message: error.to_string(),
                })?;
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
