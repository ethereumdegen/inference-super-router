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
    CreditsClient, FacilitatorClient, InferenceClient, NonceTracker, RateLimiter, SettlementQueue,
    VerificationCache,
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
use tracing::{debug, info, warn};

/// A registered endpoint with all configuration needed for dispatch.
#[derive(Clone)]
pub struct RegisteredEndpoint {
    pub endpoint: ResolvedEndpoint,
    pub client: InferenceClient,
    pub payment_config: EndpointPaymentConfig,
    pub credits_enabled: bool,
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

        Box::pin(async move {
            let mut req = req;

            // Buffer the request body to peek at the model field
            let body_bytes = drain_payload(&mut req).await;

            // Parse body as JSON and extract model
            let model_name = serde_json::from_slice::<serde_json::Value>(&body_bytes)
                .ok()
                .and_then(|json| json.get("model").and_then(|v| v.as_str()).map(String::from));

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

            // Try credits path if applicable
            let use_credits = registered.credits_enabled && credits_client.is_some();

            if use_credits && erc8128::has_erc8128_headers(req.headers()) {
                debug!("ERC-8128 headers detected, attempting credits path");
                let cc = credits_client.as_ref().unwrap();

                match erc8128::verify_from_request(req.request(), &body_bytes) {
                    Ok(identity) => {
                        let wallet = identity.wallet_address.to_lowercase();
                        info!("ERC-8128 verified for wallet: {}", wallet);

                        match cc.get_credits(&wallet).await {
                            Ok(credits) if credits > 0 => {
                                match cc.adjust_credits(&wallet, -1).await {
                                    Ok(new_balance) => {
                                        info!(
                                            "Deducted 1 credit from {}: {} remaining",
                                            wallet, new_balance
                                        );
                                        set_payload_from_bytes(&mut req, body_bytes);
                                        let res = service.call(req).await?;
                                        return Ok(res.map_into_left_body());
                                    }
                                    Err(e) => {
                                        warn!(
                                            "Failed to deduct credits for {}: {}",
                                            wallet, e
                                        );
                                        // Fall through to x402
                                    }
                                }
                            }
                            Ok(credits) => {
                                info!(
                                    "Wallet {} has {} credits, falling through to x402",
                                    wallet, credits
                                );
                            }
                            Err(e) => {
                                warn!("Credits check failed for {}: {}", wallet, e);
                            }
                        }
                    }
                    Err(e) => {
                        warn!("ERC-8128 verification failed: {}", e);
                    }
                }
            }

            // Re-attach body and fall through to x402 payment flow
            set_payload_from_bytes(&mut req, body_bytes);

            let credits_headers: [(&str, &str); 1] = [("x-erc8128-credits", "true")];
            let extra_headers: Option<&[(&str, &str)]> = if use_credits {
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
