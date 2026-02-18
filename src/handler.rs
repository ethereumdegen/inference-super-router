use crate::endpoints::ResolvedEndpoint;
use crate::error::AppError;
use crate::models::{ChatMessage, ChatRequest};
use crate::services::InferenceClient;
use actix_web::{web, HttpMessage, HttpRequest, HttpResponse};
use tracing::{debug, info, warn};

/// Estimate token count from text (chars / 4 heuristic)
fn estimate_tokens(text: &str) -> u32 {
    (text.chars().count() / 4) as u32
}

fn estimate_request_tokens(request: &ChatRequest) -> u32 {
    request
        .messages
        .iter()
        .map(|msg| {
            estimate_tokens(&msg.role)
                + msg.content.as_deref().map(estimate_tokens).unwrap_or(0)
        })
        .sum()
}

/// Unified chat handler — reads InferenceClient and ResolvedEndpoint from
/// request extensions (injected by UnifiedDispatchMiddleware).
pub async fn unified_chat_handler(
    req: HttpRequest,
    request: web::Json<ChatRequest>,
) -> Result<HttpResponse, AppError> {
    let (client, endpoint) = {
        let ext = req.extensions();
        let client = ext
            .get::<InferenceClient>()
            .cloned()
            .ok_or_else(|| AppError::Internal("Missing InferenceClient in request extensions".into()))?;
        let endpoint = ext
            .get::<ResolvedEndpoint>()
            .cloned()
            .ok_or_else(|| AppError::Internal("Missing ResolvedEndpoint in request extensions".into()))?;
        (client, endpoint)
    };

    info!("Processing chat request for endpoint: {}", endpoint.def.name);
    debug!("Chat request: {:?}", request);

    let mut final_request = request.into_inner();

    // Prepend system prompt if configured and no system message exists
    if let Some(system_prompt) = &endpoint.system_prompt {
        let has_system_message = final_request
            .messages
            .iter()
            .any(|m| m.role == "system");

        if !has_system_message {
            final_request.messages.insert(0, ChatMessage::system(system_prompt));
            info!("Prepended system prompt to request");
        }
    }

    // Set max_tokens from config if not specified
    if final_request.max_tokens.is_none() {
        final_request.max_tokens = Some(endpoint.def.max_output_tokens);
    }

    // Estimate and validate input token count
    let estimated_tokens = estimate_request_tokens(&final_request);
    info!("Estimated input tokens: {}", estimated_tokens);

    if estimated_tokens > endpoint.def.max_input_tokens {
        warn!(
            "Request rejected: estimated {} tokens exceeds limit of {}",
            estimated_tokens, endpoint.def.max_input_tokens
        );
        return Err(AppError::InputTooLarge(format!(
            "Estimated {} input tokens exceeds maximum of {}",
            estimated_tokens, endpoint.def.max_input_tokens
        )));
    }

    let response = client.chat(&final_request).await?;

    debug!("Chat response: {:?}", response);
    info!("Chat request completed successfully for endpoint: {}", endpoint.def.name);

    Ok(HttpResponse::Ok().json(response))
}
