use crate::error::AppError;
use crate::models::{ChatRequest, ChatResponse};
use reqwest::Client;
use tracing::{error, info};

/// Generic AI inference client that works with any OpenAI-compatible API.
/// Uses archetype-based protocol adjustments.
#[derive(Clone)]
pub struct InferenceClient {
    client: Client,
    endpoint: String,
    api_key: String,
    default_model: String,
    archetype: String,
}

impl InferenceClient {
    pub fn new(endpoint: &str, api_key: &str, default_model: &str, archetype: &str) -> Self {
        InferenceClient {
            client: Client::new(),
            endpoint: endpoint.to_string(),
            api_key: api_key.to_string(),
            default_model: default_model.to_string(),
            archetype: archetype.to_string(),
        }
    }

    /// Send a chat request to the upstream API
    pub async fn chat(&self, request: &ChatRequest) -> Result<ChatResponse, AppError> {
        info!("Sending chat request to {} at: {}", self.archetype, self.endpoint);

        info!("Using model: {}, messages: {}", self.default_model, request.messages.len());

        // Always use the relay's configured model
        let mut request_body = serde_json::to_value(request).map_err(|e| {
            error!("Failed to serialize request: {}", e);
            AppError::InferenceBackend(format!("Serialization failed: {}", e))
        })?;

        request_body["model"] = serde_json::Value::String(self.default_model.clone());

        // Protocol adjustments based on archetype
        match self.archetype.as_str() {
            "openai" => {
                // OpenAI uses max_completion_tokens instead of max_tokens
                if let Some(max_tokens) = request_body.get("max_tokens").and_then(|v| v.as_u64()) {
                    request_body["max_completion_tokens"] = serde_json::Value::Number(max_tokens.into());
                    request_body.as_object_mut().unwrap().remove("max_tokens");
                }
            }
            "kimi" => {
                // Kimi K2.5 has thinking enabled by default, which is incompatible with
                // tool_choice: "required". Disable thinking so tool calling works reliably.
                request_body["thinking"] = serde_json::json!({"type": "disabled"});
            }
            _ => {
                // "minimax" and others: OpenAI-compatible, no adjustments needed
            }
        }

        info!(
            ">>> API REQUEST [{}]:\n{}",
            self.archetype,
            serde_json::to_string_pretty(&request_body).unwrap_or_else(|_| format!("{:?}", request_body))
        );

        let response = self
            .client
            .post(&self.endpoint)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&request_body)
            .send()
            .await
            .map_err(|e| {
                error!("Failed to connect to {} API: {}", self.archetype, e);
                AppError::InferenceBackend(format!("Connection failed: {}", e))
            })?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            error!("{} API returned error: {} - {}", self.archetype, status, body);
            return Err(AppError::InferenceBackend(format!("API returned {}: {}", status, body)));
        }

        let chat_response: ChatResponse = response.json().await.map_err(|e| {
            error!("Failed to parse {} API response: {}", self.archetype, e);
            AppError::InferenceBackend(format!("Invalid response: {}", e))
        })?;

        info!(
            "<<< API RESPONSE [{}]:\n{}",
            self.archetype,
            serde_json::to_string_pretty(&chat_response).unwrap_or_else(|_| format!("{:?}", chat_response))
        );

        info!(
            "Response received - model: {}, choices: {}",
            chat_response.model, chat_response.choices.len()
        );

        // Log tool calls prominently
        for choice in &chat_response.choices {
            if let Some(ref tool_calls) = choice.message.tool_calls {
                for tc in tool_calls {
                    info!(">>> TOOL CALL: {} | id: {} | args: {}", tc.function.name, tc.id, tc.function.arguments);
                }
            }
        }

        if let Some(usage) = &chat_response.usage {
            info!("Token usage - prompt: {}, completion: {}, total: {}", usage.prompt_tokens, usage.completion_tokens, usage.total_tokens);
        }

        Ok(chat_response)
    }
}
