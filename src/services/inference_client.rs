use crate::error::AppError;
use crate::models::chat::{
    ChatChoice, ChatResponse, ChatUsage, FunctionCall, ResponseMessage, ToolCall,
};
use crate::models::ChatRequest;
use reqwest::Client;
use serde_json::Value;
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
        info!(
            "Sending chat request to {} at: {}",
            self.archetype, self.endpoint
        );

        info!(
            "Using model: {}, messages: {}",
            self.default_model,
            request.messages.len()
        );

        if self.archetype == "anthropic" {
            return self.chat_anthropic(request).await;
        }

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
                if let Some(max_tokens) = request_body.get("max_tokens").and_then(|v| v.as_u64())
                {
                    request_body["max_completion_tokens"] =
                        serde_json::Value::Number(max_tokens.into());
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

        // Strip internal fields that upstream APIs don't understand
        if let Some(obj) = request_body.as_object_mut() {
            obj.remove("payment_type");
        }

        info!(
            ">>> API REQUEST [{}]:\n{}",
            self.archetype,
            serde_json::to_string_pretty(&request_body)
                .unwrap_or_else(|_| format!("{:?}", request_body))
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
            error!(
                "{} API returned error: {} - {}",
                self.archetype, status, body
            );
            return Err(AppError::InferenceBackend(format!(
                "API returned {}: {}",
                status, body
            )));
        }

        let raw_body = response.text().await.map_err(|e| {
            error!(
                "Failed to read {} API response body: {}",
                self.archetype, e
            );
            AppError::InferenceBackend(format!("Failed to read response: {}", e))
        })?;

        let chat_response: ChatResponse = serde_json::from_str(&raw_body).map_err(|e| {
            error!("Failed to parse {} API response: {}", self.archetype, e);
            error!(
                "Raw response body: {}",
                &raw_body[..raw_body.len().min(2000)]
            );
            AppError::InferenceBackend(format!("Invalid response: {}", e))
        })?;

        self.log_response(&chat_response);

        Ok(chat_response)
    }

    /// Anthropic Messages API: different auth, request format, and response format.
    async fn chat_anthropic(&self, request: &ChatRequest) -> Result<ChatResponse, AppError> {
        // Build Anthropic-format request body
        let mut body = serde_json::Map::new();
        body.insert(
            "model".to_string(),
            Value::String(self.default_model.clone()),
        );

        // Extract system messages into top-level "system" field
        let mut system_parts: Vec<String> = Vec::new();
        let mut messages: Vec<Value> = Vec::new();

        for msg in &request.messages {
            if msg.role == "system" {
                if let Some(ref content) = msg.content {
                    system_parts.push(content.clone());
                }
            } else if msg.role == "tool" {
                // Anthropic expects tool results as role "user" with tool_result content block
                messages.push(serde_json::json!({
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": msg.tool_call_id.as_deref().unwrap_or(""),
                        "content": msg.content.as_deref().unwrap_or("")
                    }]
                }));
            } else if msg.role == "assistant" {
                if let Some(ref tool_calls) = msg.tool_calls {
                    // Assistant message with tool calls → content blocks
                    let mut content_blocks: Vec<Value> = Vec::new();
                    if let Some(ref text) = msg.content {
                        if !text.is_empty() {
                            content_blocks.push(serde_json::json!({"type": "text", "text": text}));
                        }
                    }
                    for tc in tool_calls {
                        let input: Value =
                            serde_json::from_str(&tc.function.arguments).unwrap_or(Value::Object(
                                serde_json::Map::new(),
                            ));
                        content_blocks.push(serde_json::json!({
                            "type": "tool_use",
                            "id": tc.id,
                            "name": tc.function.name,
                            "input": input
                        }));
                    }
                    messages.push(serde_json::json!({
                        "role": "assistant",
                        "content": content_blocks
                    }));
                } else {
                    messages.push(serde_json::json!({
                        "role": "assistant",
                        "content": msg.content.as_deref().unwrap_or("")
                    }));
                }
            } else {
                // user messages
                messages.push(serde_json::json!({
                    "role": msg.role,
                    "content": msg.content.as_deref().unwrap_or("")
                }));
            }
        }

        if !system_parts.is_empty() {
            body.insert(
                "system".to_string(),
                Value::String(system_parts.join("\n\n")),
            );
        }
        body.insert("messages".to_string(), Value::Array(messages));

        // max_tokens is required for Anthropic
        let max_tokens = request.max_tokens.unwrap_or(8192);
        body.insert(
            "max_tokens".to_string(),
            Value::Number(max_tokens.into()),
        );

        if let Some(temp) = request.temperature {
            body.insert(
                "temperature".to_string(),
                serde_json::json!(temp),
            );
        }

        // Convert OpenAI tools format to Anthropic format
        if let Some(ref tools) = request.tools {
            let anthropic_tools: Vec<Value> = tools
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "name": t.function.name,
                        "description": t.function.description,
                        "input_schema": t.function.parameters
                    })
                })
                .collect();
            body.insert("tools".to_string(), Value::Array(anthropic_tools));

            // Convert tool_choice
            if let Some(ref tc) = request.tool_choice {
                let anthropic_tc = match tc {
                    Value::String(s) if s == "auto" => serde_json::json!({"type": "auto"}),
                    Value::String(s) if s == "none" => serde_json::json!({"type": "any"}),
                    Value::String(s) if s == "required" => serde_json::json!({"type": "any"}),
                    Value::Object(obj) if obj.contains_key("function") => {
                        if let Some(name) = obj.get("function").and_then(|f| f.get("name")).and_then(|n| n.as_str()) {
                            serde_json::json!({"type": "tool", "name": name})
                        } else {
                            serde_json::json!({"type": "auto"})
                        }
                    }
                    _ => serde_json::json!({"type": "auto"}),
                };
                body.insert("tool_choice".to_string(), anthropic_tc);
            }
        }

        let request_body = Value::Object(body);

        info!(
            ">>> API REQUEST [anthropic]:\n{}",
            serde_json::to_string_pretty(&request_body)
                .unwrap_or_else(|_| format!("{:?}", request_body))
        );

        let response = self
            .client
            .post(&self.endpoint)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("Content-Type", "application/json")
            .json(&request_body)
            .send()
            .await
            .map_err(|e| {
                error!("Failed to connect to Anthropic API: {}", e);
                AppError::InferenceBackend(format!("Connection failed: {}", e))
            })?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            error!("Anthropic API returned error: {} - {}", status, body);
            return Err(AppError::InferenceBackend(format!(
                "API returned {}: {}",
                status, body
            )));
        }

        let raw_body = response.text().await.map_err(|e| {
            error!("Failed to read Anthropic API response body: {}", e);
            AppError::InferenceBackend(format!("Failed to read response: {}", e))
        })?;

        info!("<<< RAW ANTHROPIC RESPONSE:\n{}", &raw_body[..raw_body.len().min(2000)]);

        // Parse Anthropic response and convert to OpenAI-compatible format
        let anthropic_resp: Value = serde_json::from_str(&raw_body).map_err(|e| {
            error!("Failed to parse Anthropic response: {}", e);
            AppError::InferenceBackend(format!("Invalid response: {}", e))
        })?;

        let chat_response = self.anthropic_to_openai_response(&anthropic_resp)?;

        self.log_response(&chat_response);

        Ok(chat_response)
    }

    /// Convert Anthropic Messages API response to OpenAI-compatible ChatResponse.
    fn anthropic_to_openai_response(&self, resp: &Value) -> Result<ChatResponse, AppError> {
        let id = resp
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("msg_unknown")
            .to_string();
        let model = resp
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or(&self.default_model)
            .to_string();

        // Extract text content and tool_use blocks from content array
        let mut text_parts: Vec<String> = Vec::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();

        if let Some(content) = resp.get("content").and_then(|v| v.as_array()) {
            for block in content {
                match block.get("type").and_then(|v| v.as_str()) {
                    Some("text") => {
                        if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                            text_parts.push(text.to_string());
                        }
                    }
                    Some("tool_use") => {
                        let tc_id = block
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("call_unknown")
                            .to_string();
                        let name = block
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let input = block.get("input").cloned().unwrap_or(Value::Object(
                            serde_json::Map::new(),
                        ));
                        tool_calls.push(ToolCall {
                            id: tc_id,
                            call_type: "function".to_string(),
                            function: FunctionCall {
                                name,
                                arguments: serde_json::to_string(&input).unwrap_or_default(),
                            },
                        });
                    }
                    _ => {}
                }
            }
        }

        let content = if text_parts.is_empty() {
            None
        } else {
            Some(text_parts.join(""))
        };

        let tool_calls_opt = if tool_calls.is_empty() {
            None
        } else {
            Some(tool_calls)
        };

        // Map Anthropic stop_reason to OpenAI finish_reason
        let finish_reason = match resp.get("stop_reason").and_then(|v| v.as_str()) {
            Some("end_turn") => Some("stop".to_string()),
            Some("tool_use") => Some("tool_calls".to_string()),
            Some("max_tokens") => Some("length".to_string()),
            Some("stop_sequence") => Some("stop".to_string()),
            Some(other) => Some(other.to_string()),
            None => None,
        };

        // Convert usage
        let usage = resp.get("usage").map(|u| ChatUsage {
            prompt_tokens: u
                .get("input_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32,
            completion_tokens: u
                .get("output_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32,
            total_tokens: (u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0)
                + u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0))
                as u32,
        });

        Ok(ChatResponse {
            id,
            object: "chat.completion".to_string(),
            created: 0,
            model,
            choices: vec![ChatChoice {
                index: 0,
                message: ResponseMessage {
                    role: "assistant".to_string(),
                    content,
                    tool_calls: tool_calls_opt,
                },
                finish_reason,
            }],
            usage,
        })
    }

    fn log_response(&self, chat_response: &ChatResponse) {
        info!(
            "<<< API RESPONSE [{}]:\n{}",
            self.archetype,
            serde_json::to_string_pretty(&chat_response)
                .unwrap_or_else(|_| format!("{:?}", chat_response))
        );

        info!(
            "Response received - model: {}, choices: {}",
            chat_response.model,
            chat_response.choices.len()
        );

        // Log tool calls prominently
        for choice in &chat_response.choices {
            if let Some(ref tool_calls) = choice.message.tool_calls {
                for tc in tool_calls {
                    info!(
                        ">>> TOOL CALL: {} | id: {} | args: {}",
                        tc.function.name, tc.id, tc.function.arguments
                    );
                }
            }
        }

        if let Some(usage) = &chat_response.usage {
            info!(
                "Token usage - prompt: {}, completion: {}, total: {}",
                usage.prompt_tokens, usage.completion_tokens, usage.total_tokens
            );
        }
    }
}
