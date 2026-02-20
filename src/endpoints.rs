use serde::Deserialize;

/// Root RON config structure
#[derive(Debug, Clone, Deserialize)]
pub struct EndpointsConfig {
    pub endpoints: Vec<EndpointDef>,
}

/// Definition of a single inference endpoint from endpoints.ron
#[derive(Debug, Clone, Deserialize)]
pub struct EndpointDef {
    /// Human-readable name (e.g. "kimi-k2.5")
    pub name: String,
    /// Route prefix (e.g. "/kimi") — registers {prefix}/chat and {prefix}/api/v1/chat/completions
    pub route_prefix: String,
    /// Upstream AI API endpoint URL
    pub api_endpoint: String,
    /// Name of the env var holding the API key (e.g. "KIMI_API_KEY")
    pub api_key_env: String,
    /// Model name to send to the upstream API
    pub model: String,
    /// Protocol archetype: "kimi", "openai", or "minimax"
    pub archetype: String,
    /// Cost per request in raw units (for USDC) or human-readable (for Starkbot)
    pub cost: String,
    /// Payment currency: "usdc" (x402 v2) or "starkbot" (x402 v1)
    pub payment_currency: String,
    /// Maximum input tokens
    pub max_input_tokens: u32,
    /// Maximum output tokens
    pub max_output_tokens: u32,
    /// Optional path to a system prompt file
    pub system_prompt_file: Option<String>,
    /// Human-readable description shown in 402 responses
    pub description: String,
    /// Credit cost per request in raw USDC units (1,000,000 = $1). 0 = credits not accepted.
    #[serde(default)]
    pub credit_cost: i64,
}

/// A resolved endpoint with API key and system prompt loaded at startup
#[derive(Debug, Clone)]
pub struct ResolvedEndpoint {
    pub def: EndpointDef,
    /// Resolved API key value
    pub api_key: String,
    /// Loaded system prompt text
    pub system_prompt: Option<String>,
}

/// Load and parse endpoints from a RON config file
pub fn load_endpoints(path: &str) -> EndpointsConfig {
    let content = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("Failed to read endpoints config '{}': {}", path, e));
    ron::from_str(&content)
        .unwrap_or_else(|e| panic!("Failed to parse endpoints config '{}': {}", path, e))
}

/// Resolve an EndpointDef by loading its API key from env and system prompt from file
pub fn resolve_endpoint(def: EndpointDef) -> Result<ResolvedEndpoint, String> {
    let api_key = std::env::var(&def.api_key_env)
        .map_err(|_| format!("Missing env var {} for endpoint '{}'", def.api_key_env, def.name))?;

    let system_prompt = match &def.system_prompt_file {
        Some(path) => {
            let content = std::fs::read_to_string(path)
                .map_err(|e| format!("Failed to read system prompt '{}' for endpoint '{}': {}", path, def.name, e))?;
            let trimmed = content.trim().to_string();
            if trimmed.is_empty() { None } else { Some(trimmed) }
        }
        None => None,
    };

    Ok(ResolvedEndpoint {
        def,
        api_key,
        system_prompt,
    })
}
