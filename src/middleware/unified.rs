//! Unified dispatch middleware.
//!
//! Buffers the request body, peeks at the `model` field, resolves the correct
//! endpoint/client/payment config from the `EndpointRegistry`, then runs the
//! appropriate payment flow (credits or x402) before forwarding to the handler.

use super::x402::handle_x402_payment;
use crate::config::GlobalConfig;
use crate::endpoints::ResolvedEndpoint;
use crate::erc8128;
use crate::payment::EndpointPaymentConfig;
use crate::services::{
    CreditsClient, FacilitatorClient, InferenceClient, NonceTracker, RateLimiter, SessionManager,
    SettlementQueue, VerificationCache,
};
use actix_web::{
    body::EitherBody,
    dev::{forward_ready, Service, ServiceRequest, ServiceResponse, Transform},
    web::{Bytes, BytesMut},
    Error, HttpMessage, HttpResponse,
};
use futures_core::Stream;
use std::collections::HashMap;
use std::future::{poll_fn, ready, Future, Ready};
use std::pin::Pin;
use std::sync::Arc;
use tracing::{info, warn};

/// A registered endpoint with all configuration needed for dispatch.
#[derive(Clone)]
pub struct RegisteredEndpoint {
    pub endpoint: ResolvedEndpoint,
    pub client: InferenceClient,
    pub payment_config: EndpointPaymentConfig,
    pub credit_cost: i64,
}

/// Registry of all available endpoints, keyed by model name.
#[derive(Clone)]
pub struct EndpointRegistry {
    pub models: HashMap<String, RegisteredEndpoint>,
    pub default_model: String,
}

impl EndpointRegistry {
    /// Case-insensitive model lookup. Tries exact match first, then lowercase.
    pub fn lookup(&self, key: &str) -> Option<&RegisteredEndpoint> {
        self.models.get(key).or_else(|| {
            let lower = key.to_lowercase();
            self.models
                .iter()
                .find(|(k, _)| k.to_lowercase() == lower)
                .map(|(_, v)| v)
        })
    }
}

pub struct UnifiedDispatchMiddleware {
    global_config: GlobalConfig,
    registry: Arc<EndpointRegistry>,
    facilitator: FacilitatorClient,
    nonce_tracker: Arc<NonceTracker>,
    settlement_queue: Arc<SettlementQueue>,
    rate_limiter: Arc<RateLimiter>,
    verification_cache: Arc<VerificationCache>,
    credits_client: Option<Arc<CreditsClient>>,
    session_manager: Arc<SessionManager>,
}

impl UnifiedDispatchMiddleware {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        global_config: GlobalConfig,
        registry: Arc<EndpointRegistry>,
        facilitator: FacilitatorClient,
        nonce_tracker: Arc<NonceTracker>,
        settlement_queue: Arc<SettlementQueue>,
        rate_limiter: Arc<RateLimiter>,
        verification_cache: Arc<VerificationCache>,
        credits_client: Option<Arc<CreditsClient>>,
        session_manager: Arc<SessionManager>,
    ) -> Self {
        Self {
            global_config,
            registry,
            facilitator,
            nonce_tracker,
            settlement_queue,
            rate_limiter,
            verification_cache,
            credits_client,
            session_manager,
        }
    }
}

impl<S, B> Transform<S, ServiceRequest> for UnifiedDispatchMiddleware
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    S::Future: 'static,
    B: 'static,
{
    type Response = ServiceResponse<EitherBody<B>>;
    type Error = Error;
    type InitError = ();
    type Transform = UnifiedDispatchMiddlewareService<S>;
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        ready(Ok(UnifiedDispatchMiddlewareService {
            service: Arc::new(service),
            global_config: self.global_config.clone(),
            registry: self.registry.clone(),
            facilitator: self.facilitator.clone(),
            nonce_tracker: self.nonce_tracker.clone(),
            settlement_queue: self.settlement_queue.clone(),
            rate_limiter: self.rate_limiter.clone(),
            verification_cache: self.verification_cache.clone(),
            credits_client: self.credits_client.clone(),
            session_manager: self.session_manager.clone(),
        }))
    }
}

pub struct UnifiedDispatchMiddlewareService<S> {
    service: Arc<S>,
    global_config: GlobalConfig,
    registry: Arc<EndpointRegistry>,
    facilitator: FacilitatorClient,
    nonce_tracker: Arc<NonceTracker>,
    settlement_queue: Arc<SettlementQueue>,
    rate_limiter: Arc<RateLimiter>,
    verification_cache: Arc<VerificationCache>,
    credits_client: Option<Arc<CreditsClient>>,
    session_manager: Arc<SessionManager>,
}

impl<S, B> Service<ServiceRequest> for UnifiedDispatchMiddlewareService<S>
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    S::Future: 'static,
    B: 'static,
{
    type Response = ServiceResponse<EitherBody<B>>;
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>>>>;

    forward_ready!(service);

    fn call(&self, req: ServiceRequest) -> Self::Future {
        let service = Arc::clone(&self.service);
        let global_config = self.global_config.clone();
        let registry = self.registry.clone();
        let facilitator = self.facilitator.clone();
        let nonce_tracker = self.nonce_tracker.clone();
        let settlement_queue = self.settlement_queue.clone();
        let rate_limiter = self.rate_limiter.clone();
        let verification_cache = self.verification_cache.clone();
        let credits_client = self.credits_client.clone();
        let session_manager = self.session_manager.clone();

        Box::pin(async move {
            let mut req = req;

            // Buffer the request body to peek at the model field
            let body_bytes = drain_payload(&mut req).await;

            // Parse body as JSON and extract model + payment_type
            let parsed_body = serde_json::from_slice::<serde_json::Value>(&body_bytes).ok();
            let model_name = parsed_body
                .as_ref()
                .and_then(|json| json.get("model").and_then(|v| v.as_str()).map(String::from));
            let payment_type = parsed_body
                .as_ref()
                .and_then(|json| json.get("payment_type").and_then(|v| v.as_str()).map(String::from));

            // Resolve model name: None, empty, or "auto" → default_model
            let model_key = match model_name.as_deref() {
                None | Some("") | Some("auto") => registry.default_model.clone(),
                Some(name) => name.to_string(),
            };

            // Look up model in registry (case-insensitive)
            let registered = match registry.lookup(&model_key) {
                Some(r) => r.clone(),
                None => {
                    let mut available: Vec<&str> =
                        registry.models.keys().map(|s| s.as_str()).collect();
                    available.sort();
                    available.push("auto");
                    let response = HttpResponse::BadRequest().json(serde_json::json!({
                        "error": format!("Unknown model: '{}'. Available models: {:?}", model_key, available),
                        "available_models": available,
                    }));
                    return Ok(req.into_response(response).map_into_right_body());
                }
            };

            info!(
                "Unified dispatch: model='{}' -> endpoint='{}'",
                model_key, registered.endpoint.def.name
            );

            // Insert client and endpoint into request extensions for the handler
            req.extensions_mut().insert(registered.client.clone());
            req.extensions_mut().insert(registered.endpoint.clone());

            // Resolve payment_type: None, empty, or "auto" → auto
            let pay_mode = match payment_type.as_deref() {
                None | Some("") | Some("auto") => "auto",
                Some("credits") => "credits",
                Some("x402") if !global_config.x402_enabled => {
                    let response = HttpResponse::BadRequest().json(serde_json::json!({
                        "error": "x402 on-chain payment is currently disabled. Please use credits.",
                    }));
                    return Ok(req.into_response(response).map_into_right_body());
                }
                Some("x402") => "x402",
                Some(other) => {
                    let response = HttpResponse::BadRequest().json(serde_json::json!({
                        "error": format!(
                            "Unknown payment_type: '{}'. Valid values: \"auto\", \"credits\", \"x402\"",
                            other
                        ),
                    }));
                    return Ok(req.into_response(response).map_into_right_body());
                }
            };

            info!(
                "Unified dispatch: payment_type='{}', model='{}'",
                pay_mode, model_key
            );

            // Check if credits path is viable for this endpoint
            let credit_cost = registered.credit_cost;
            let credits_available = credit_cost > 0 && credits_client.is_some();
            let has_erc8128 = erc8128::has_erc8128_headers(req.headers());
            let bearer_token = extract_bearer_token(req.headers());
            let has_bearer = bearer_token.is_some();

            // If client explicitly requested credits but this endpoint doesn't support them
            if pay_mode == "credits" && !credits_available {
                let response = HttpResponse::BadRequest().json(serde_json::json!({
                    "error": format!(
                        "Model '{}' does not support credits payment. Use payment_type \"x402\" or \"auto\".",
                        model_key
                    ),
                }));
                return Ok(req.into_response(response).map_into_right_body());
            }

            info!(
                "[CREDITS] credit_cost={}, credits_client={}, has_erc8128={}, has_bearer={}, model={}, pay_mode={}",
                credit_cost,
                credits_client.is_some(),
                has_erc8128,
                has_bearer,
                model_key,
                pay_mode
            );

            // Try credits path if applicable:
            //   - Bearer token present (session-based) OR ERC-8128 headers present
            //   - AND credits are available for this endpoint
            //   - AND pay_mode is "credits" or "auto"
            let try_credits = credits_available
                && (pay_mode == "credits"
                    || (pay_mode == "auto" && (has_bearer || has_erc8128)));

            if try_credits {
                // Resolve wallet address: Bearer token (fast path) or ERC-8128 signature
                let wallet_result = if let Some(ref token) = bearer_token {
                    match session_manager.validate(token) {
                        Some(info) => {
                            info!("[CREDITS] Bearer session validated for wallet: {}", info.wallet_address);
                            Ok(info.wallet_address)
                        }
                        None => {
                            // Invalid/expired token
                            if pay_mode == "credits" {
                                let response = HttpResponse::Unauthorized().json(serde_json::json!({
                                    "error": "Invalid or expired session token",
                                }));
                                return Ok(req.into_response(response).map_into_right_body());
                            }
                            Err("Invalid or expired session token".to_string())
                        }
                    }
                } else if has_erc8128 {
                    match erc8128::verify_from_request(req.request(), &body_bytes) {
                        Ok(identity) => {
                            let wallet = identity.wallet_address.to_lowercase();
                            info!("[CREDITS] ERC-8128 verified for wallet: {} (chain: {})", wallet, identity.chain_id);
                            Ok(wallet)
                        }
                        Err(e) => {
                            warn!("[CREDITS] ERC-8128 verification failed: {}", e);
                            if pay_mode == "credits" {
                                let response = HttpResponse::Unauthorized().json(serde_json::json!({
                                    "error": format!("ERC-8128 signature verification failed: {}", e),
                                }));
                                return Ok(req.into_response(response).map_into_right_body());
                            }
                            Err(e.to_string())
                        }
                    }
                } else {
                    // credits mode but no auth
                    if pay_mode == "credits" {
                        let response = HttpResponse::BadRequest().json(serde_json::json!({
                            "error": "payment_type \"credits\" requires a Bearer session token or ERC-8128 signed headers.",
                        }));
                        return Ok(req.into_response(response).map_into_right_body());
                    }
                    Err("No credentials".to_string())
                };

                if let Ok(wallet) = wallet_result {
                    info!("[CREDITS] Attempting credits deduction for {} (cost={})", wallet, credit_cost);
                    let cc = credits_client.as_ref().unwrap();

                    match cc.get_credits(&wallet).await {
                        Ok(credits) if credits >= credit_cost => {
                            info!("[CREDITS] Wallet {} has {} credits, deducting {}", wallet, credits, credit_cost);
                            match cc.adjust_credits(&wallet, -credit_cost).await {
                                Ok(new_balance) => {
                                    info!(
                                        "[CREDITS] SUCCESS: deducted {} credits from {}: {} remaining",
                                        credit_cost, wallet, new_balance
                                    );
                                    set_payload_from_bytes(&mut req, body_bytes);
                                    let res = service.call(req).await?;
                                    return Ok(res.map_into_left_body());
                                }
                                Err(e) => {
                                    warn!("[CREDITS] Failed to deduct credits for {}: {}", wallet, e);
                                    if pay_mode == "credits" {
                                        let response = HttpResponse::InternalServerError().json(serde_json::json!({
                                            "error": format!("Credits deduction failed: {}", e),
                                        }));
                                        return Ok(req.into_response(response).map_into_right_body());
                                    }
                                    // auto: fall through to x402
                                }
                            }
                        }
                        Ok(credits) => {
                            if pay_mode == "credits" {
                                let response = HttpResponse::PaymentRequired().json(serde_json::json!({
                                    "error": format!(
                                        "Insufficient credits: have {}, need {} for model '{}'",
                                        credits, credit_cost, model_key
                                    ),
                                }));
                                return Ok(req.into_response(response).map_into_right_body());
                            }
                            info!(
                                "[CREDITS] Wallet {} has {} credits (need {}, insufficient), falling through to x402",
                                wallet, credits, credit_cost
                            );
                        }
                        Err(e) => {
                            warn!("[CREDITS] Credits check failed for {}: {}", wallet, e);
                            if pay_mode == "credits" {
                                let response = HttpResponse::InternalServerError().json(serde_json::json!({
                                    "error": format!("Credits check failed: {}", e),
                                }));
                                return Ok(req.into_response(response).map_into_right_body());
                            }
                        }
                    }
                }
            } else if pay_mode == "auto" && credits_available {
                info!("[CREDITS] No auth headers in request, skipping credits path");
            }

            // If client explicitly requested credits, we would have returned above.
            // If we're here with pay_mode == "credits", something unexpected happened.
            if pay_mode == "credits" {
                let response = HttpResponse::InternalServerError().json(serde_json::json!({
                    "error": "Credits payment failed unexpectedly.",
                }));
                return Ok(req.into_response(response).map_into_right_body());
            }

            // x402 disabled — reject requests that didn't pay via credits
            if !global_config.x402_enabled {
                let response = HttpResponse::PaymentRequired().json(serde_json::json!({
                    "error": "x402 on-chain payment is currently disabled. Please use credits.",
                }));
                return Ok(req.into_response(response).map_into_right_body());
            }

            // Re-attach body and proceed to x402 payment flow
            set_payload_from_bytes(&mut req, body_bytes);

            let credits_headers: [(&str, &str); 1] = [("x-erc8128-credits", "true")];
            let extra_headers: Option<&[(&str, &str)]> = if credits_available {
                Some(&credits_headers)
            } else {
                None
            };

            handle_x402_payment(
                req,
                service,
                &global_config,
                &registered.payment_config,
                &facilitator,
                &nonce_tracker,
                &settlement_queue,
                &rate_limiter,
                &verification_cache,
                extra_headers,
            )
            .await
        })
    }
}

/// Drain the payload stream from a ServiceRequest into a Vec<u8>.
async fn drain_payload(req: &mut ServiceRequest) -> Vec<u8> {
    let mut payload = req.take_payload();
    let mut buf = BytesMut::new();

    loop {
        let chunk = poll_fn(|cx| Pin::new(&mut payload).poll_next(cx)).await;
        match chunk {
            Some(Ok(bytes)) => buf.extend_from_slice(&bytes),
            _ => break,
        }
    }

    buf.to_vec()
}

/// Re-attach a byte buffer as the request payload.
fn set_payload_from_bytes(req: &mut ServiceRequest, body: Vec<u8>) {
    let (_, mut pl) = actix_http::h1::Payload::create(true);
    pl.unread_data(Bytes::from(body));
    req.set_payload(pl.into());
}

/// Extract a Bearer token from the Authorization header.
fn extract_bearer_token(headers: &actix_web::http::header::HeaderMap) -> Option<String> {
    headers
        .get(actix_web::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|s| s.to_string())
}
