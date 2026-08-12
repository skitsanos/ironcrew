use serde_json::{Value, json};

pub(super) struct WebScrapePolicy {
    allowed_domains: Option<Vec<String>>,
    max_html_bytes: usize,
    allow_private: bool,
}

impl WebScrapePolicy {
    pub(super) fn capture(mut allowed_domains: Option<Vec<String>>) -> Self {
        if let Some(domains) = &mut allowed_domains {
            for domain in domains.iter_mut() {
                *domain = domain.to_ascii_lowercase();
            }
            domains.sort_unstable();
            domains.dedup();
        }
        Self {
            allowed_domains,
            max_html_bytes: crate::utils::http::byte_limit_from_env(
                "IRONCREW_WEB_SCRAPE_MAX_BYTES",
                crate::utils::http::DEFAULT_WEB_SCRAPE_BYTES,
            ),
            allow_private: crate::utils::network::private_ips_override_enabled(),
        }
    }

    #[cfg(test)]
    pub(super) fn from_values(
        mut allowed_domains: Option<Vec<String>>,
        max_html_bytes: usize,
        allow_private: bool,
    ) -> Self {
        if let Some(domains) = &mut allowed_domains {
            for domain in domains.iter_mut() {
                *domain = domain.to_ascii_lowercase();
            }
            domains.sort_unstable();
            domains.dedup();
        }
        Self {
            allowed_domains,
            max_html_bytes,
            allow_private,
        }
    }

    pub(super) fn allowed_domains(&self) -> Option<&[String]> {
        self.allowed_domains.as_deref()
    }

    pub(super) fn max_html_bytes(&self) -> usize {
        self.max_html_bytes
    }

    pub(super) fn allow_private(&self) -> bool {
        self.allow_private
    }

    pub(super) fn definition(&self) -> Value {
        let domains = self.allowed_domains.as_deref().unwrap_or_default();
        json!({
            "domain_filter_enabled": self.allowed_domains.is_some(),
            "allowed_domains_fingerprint":
                crate::tools::execution_policy::strings_fingerprint("web-scrape-domains", domains),
            "max_html_bytes": self.max_html_bytes,
            "allow_private": self.allow_private,
        })
    }
}
