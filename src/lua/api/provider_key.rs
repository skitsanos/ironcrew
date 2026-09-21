pub(super) fn trusted_provider_key_env_name(base_url: &str) -> Option<&'static str> {
    let parsed = reqwest::Url::parse(base_url).ok()?;
    if parsed.scheme() != "https" {
        return None;
    }
    match parsed.host_str()?.to_ascii_lowercase().as_str() {
        "api.openai.com" => Some("OPENAI_API_KEY"),
        "generativelanguage.googleapis.com" => Some("GEMINI_API_KEY"),
        "api.groq.com" => Some("GROQ_API_KEY"),
        "api.moonshot.ai" | "api.moonshot.cn" => Some("MOONSHOT_API_KEY"),
        "api.deepseek.com" => Some("DEEPSEEK_API_KEY"),
        "api.x.ai" => Some("XAI_API_KEY"),
        "api.openrouter.ai" => Some("OPENROUTER_API_KEY"),
        "api.anthropic.com" => Some("ANTHROPIC_API_KEY"),
        _ => None,
    }
}
