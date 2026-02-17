use crate::config::GlobalConfig;
use crate::payment::EndpointPaymentConfig;
use crate::services::{
    FacilitatorClient, NonceTracker, PendingSettlement, RateLimiter, SettlementQueue,
    VerificationCache,
};
use actix_web::{
    body::EitherBody,
    dev::{forward_ready, Service, ServiceRequest, ServiceResponse, Transform},
    http::header::{HeaderName, HeaderValue},
    Error, HttpResponse,
};
use base64::Engine;
use std::future::{ready, Future, Ready};
use std::pin::Pin;
use std::sync::Arc;
use tracing::{debug, error, info, warn};

const X_PAYMENT_HEADER: &str = "X-PAYMENT";

/// Unified x402 middleware supporting both v1 (Starkbot/permit) and v2 (USDC/exact).
pub struct X402Middleware {
    global_config: GlobalConfig,
    payment_config: EndpointPaymentConfig,
    facilitator: FacilitatorClient,
    nonce_tracker: Arc<NonceTracker>,
    settlement_queue: Arc<SettlementQueue>,
    rate_limiter: Arc<RateLimiter>,
    verification_cache: Arc<VerificationCache>,
}

impl X402Middleware {
    pub fn new(
        global_config: GlobalConfig,
        payment_config: EndpointPaymentConfig,
        facilitator: FacilitatorClient,
        nonce_tracker: Arc<NonceTracker>,
        settlement_queue: Arc<SettlementQueue>,
        rate_limiter: Arc<RateLimiter>,
        verification_cache: Arc<VerificationCache>,
    ) -> Self {
        X402Middleware {
            global_config,
            payment_config,
            facilitator,
            nonce_tracker,
            settlement_queue,
            rate_limiter,
            verification_cache,
        }
    }
}

impl<S, B> Transform<S, ServiceRequest> for X402Middleware
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    S::Future: 'static,
    B: 'static,
{
    type Response = ServiceResponse<EitherBody<B>>;
    type Error = Error;
    type InitError = ();
    type Transform = X402MiddlewareService<S>;
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        ready(Ok(X402MiddlewareService {
            service: Arc::new(service),
            global_config: self.global_config.clone(),
            payment_config: self.payment_config.clone(),
            facilitator: self.facilitator.clone(),
            nonce_tracker: self.nonce_tracker.clone(),
            settlement_queue: self.settlement_queue.clone(),
            rate_limiter: self.rate_limiter.clone(),
            verification_cache: self.verification_cache.clone(),
        }))
    }
}

pub struct X402MiddlewareService<S> {
    service: Arc<S>,
    global_config: GlobalConfig,
    payment_config: EndpointPaymentConfig,
    facilitator: FacilitatorClient,
    nonce_tracker: Arc<NonceTracker>,
    settlement_queue: Arc<SettlementQueue>,
    rate_limiter: Arc<RateLimiter>,
    verification_cache: Arc<VerificationCache>,
}

impl<S, B> Service<ServiceRequest> for X402MiddlewareService<S>
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
        let payment_config = self.payment_config.clone();
        let facilitator = self.facilitator.clone();
        let nonce_tracker = self.nonce_tracker.clone();
        let settlement_queue = self.settlement_queue.clone();
        let rate_limiter = self.rate_limiter.clone();
        let verification_cache = self.verification_cache.clone();

        Box::pin(async move {
            // TEST_MODE: skip payment entirely
            if global_config.test_mode {
                debug!("TEST_MODE: skipping x402 payment");
                let res = service.call(req).await?;
                return Ok(res.map_into_left_body());
            }

            let resource = req.path().to_string();

            let payment_header = req
                .headers()
                .get(X_PAYMENT_HEADER)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());

            match payment_header {
                None => {
                    // No payment header — return 402
                    info!("No payment header, returning 402 for {}", resource);

                    if payment_config.is_v2() {
                        // v2: base64-encoded header
                        let encoded = match payment_config.build_402_header(&resource) {
                            Some(e) => e,
                            None => {
                                let response = HttpResponse::InternalServerError()
                                    .body("Failed to generate payment requirements");
                                return Ok(req.into_response(response).map_into_right_body());
                            }
                        };

                        let response = HttpResponse::PaymentRequired()
                            .insert_header((
                                HeaderName::from_static("payment-required"),
                                HeaderValue::from_str(&encoded)
                                    .unwrap_or_else(|_| HeaderValue::from_static("")),
                            ))
                            .body("Payment required. See payment-required header for details.");

                        Ok(req.into_response(response).map_into_right_body())
                    } else {
                        // v1: JSON body with token metadata
                        let body = payment_config.build_402_body(&resource);
                        let response = HttpResponse::PaymentRequired()
                            .content_type("application/json")
                            .json(body);
                        Ok(req.into_response(response).map_into_right_body())
                    }
                }
                Some(payment_header_value) => {
                    debug!("Payment header present, verifying...");

                    // Decode payment payload
                    let raw_payload: serde_json::Value = match base64::engine::general_purpose::STANDARD
                        .decode(&payment_header_value)
                    {
                        Ok(bytes) => match serde_json::from_slice(&bytes) {
                            Ok(v) => v,
                            Err(e) => {
                                warn!("Invalid payment JSON: {}", e);
                                let response = HttpResponse::PaymentRequired()
                                    .body(format!("Invalid payment JSON: {}", e));
                                return Ok(req.into_response(response).map_into_right_body());
                            }
                        },
                        Err(e) => {
                            warn!("Invalid payment base64: {}", e);
                            let response = HttpResponse::PaymentRequired()
                                .body(format!("Invalid payment encoding: {}", e));
                            return Ok(req.into_response(response).map_into_right_body());
                        }
                    };

                    // Extract payer and nonce (reliable for v2, best-effort for v1)
                    let payer_address = payment_config.extract_payer(&raw_payload);
                    let nonce = payment_config.extract_nonce(&raw_payload);

                    // For v2: enforce rate limiting and nonce protection
                    if payment_config.is_v2() {
                        if let Some(ref payer) = payer_address {
                            if !rate_limiter.check_rate_limit(payer) {
                                warn!("Rate limit exceeded for address: {}", payer);
                                let response = HttpResponse::TooManyRequests()
                                    .insert_header(("Retry-After", "1"))
                                    .body("Rate limit exceeded: maximum 5 requests per second per address");
                                return Ok(req.into_response(response).map_into_right_body());
                            }
                        }

                        if let Some(ref nonce_val) = nonce {
                            if !nonce_tracker.try_use_nonce(nonce_val) {
                                warn!("Replay attack detected! Nonce already used: {}", nonce_val);
                                let response = HttpResponse::PaymentRequired()
                                    .body("Payment rejected: nonce already used");
                                return Ok(req.into_response(response).map_into_right_body());
                            }
                        }
                    }

                    // Build verify request
                    let verify_request = payment_config.build_verify_request(&raw_payload, &resource);

                    // Settlement nonce: use extracted nonce or generate a unique one
                    let settlement_nonce = nonce.unwrap_or_else(|| {
                        format!("gen-{}", chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0))
                    });

                    // Build settle request for queueing
                    let settle_request = payment_config.build_settle_request(&raw_payload, &resource);

                    // For v1 (Starkbot) where payer extraction is unreliable: always take Path C
                    // For v2 (USDC): use the three-path architecture
                    if payment_config.is_v2() {
                        if let Some(ref payer) = payer_address {
                            // ── Path A: Cache hit — skip verify ──
                            if verification_cache.is_verified(payer) {
                                debug!("Cache hit for payer {}, skipping verify", payer);

                                let pending = PendingSettlement::new(settle_request, settlement_nonce.clone());
                                if let Err(_rejected) = settlement_queue.push(pending).await {
                                    error!("Settlement queue full, rejecting request");
                                    let response = HttpResponse::ServiceUnavailable()
                                        .body("Service temporarily unavailable: settlement queue full");
                                    return Ok(req.into_response(response).map_into_right_body());
                                }

                                let res = service.call(req).await?;

                                let payment_response = make_payment_response_header(true, None);
                                let (req, response) = res.into_parts();
                                let mut response = response.map_into_left_body();
                                if let Ok(hv) = HeaderValue::from_str(&payment_response) {
                                    response.headers_mut().insert(
                                        HeaderName::from_static("payment-response"), hv,
                                    );
                                }
                                return Ok(ServiceResponse::new(req, response));
                            }

                            // Pre-check queue capacity
                            if settlement_queue.is_full() {
                                error!("Settlement queue full, rejecting request early");
                                let response = HttpResponse::ServiceUnavailable()
                                    .body("Service temporarily unavailable: settlement queue full");
                                return Ok(req.into_response(response).map_into_right_body());
                            }

                            // ── Path B: Recent failure — sequential verify ──
                            if verification_cache.has_recent_failure(payer) {
                                debug!("Downgrading payer {} to sequential path", payer);

                                let verify_result = facilitator.verify_raw(&verify_request).await;

                                match verify_result {
                                    Ok(vr) if vr.is_valid => {
                                        info!("Payment verified (sequential) for payer: {:?}", vr.payer);
                                        verification_cache.mark_verified(payer);

                                        let pending = PendingSettlement::new(settle_request, settlement_nonce);
                                        if let Err(_) = settlement_queue.push(pending).await {
                                            warn!("Settlement not queued due to full queue");
                                        }

                                        let res = service.call(req).await?;
                                        let payment_response = make_payment_response_header(true, None);
                                        let (req, response) = res.into_parts();
                                        let mut response = response.map_into_left_body();
                                        if let Ok(hv) = HeaderValue::from_str(&payment_response) {
                                            response.headers_mut().insert(
                                                HeaderName::from_static("payment-response"), hv,
                                            );
                                        }
                                        return Ok(ServiceResponse::new(req, response));
                                    }
                                    Ok(vr) => {
                                        let error_msg = vr.invalid_reason.unwrap_or_else(|| "Payment verification failed".to_string());
                                        warn!("Payment verification failed (sequential): {}", error_msg);
                                        verification_cache.record_failure(payer);

                                        let payment_response = make_payment_response_header(false, Some(&error_msg));
                                        let response = HttpResponse::PaymentRequired()
                                            .insert_header((
                                                HeaderName::from_static("payment-response"),
                                                HeaderValue::from_str(&payment_response)
                                                    .unwrap_or_else(|_| HeaderValue::from_static("")),
                                            ))
                                            .body(format!("Payment verification failed: {}", error_msg));
                                        return Ok(req.into_response(response).map_into_right_body());
                                    }
                                    Err(e) => {
                                        error!("Facilitator error (sequential): {}", e);
                                        let response = HttpResponse::BadGateway()
                                            .body(format!("Facilitator error: {}", e));
                                        return Ok(req.into_response(response).map_into_right_body());
                                    }
                                }
                            }
                        }
                    }

                    // ── Path C: Parallel verify + service call ──
                    // Used for: v2 first-timers, all v1 requests
                    debug!("Verifying + calling service in parallel");

                    let verify_fut = facilitator.verify_raw(&verify_request);
                    let service_fut = service.call(req);

                    let (verify_result, service_result) = tokio::join!(verify_fut, service_fut);

                    match verify_result {
                        Ok(vr) if vr.is_valid => {
                            info!("Payment verified (parallel) for payer: {:?}", vr.payer);

                            if let Some(ref payer) = payer_address {
                                verification_cache.mark_verified(payer);
                            }

                            let pending = PendingSettlement::new(settle_request, settlement_nonce);
                            if let Err(_) = settlement_queue.push(pending).await {
                                warn!("Settlement not queued due to full queue");
                            }

                            let res = service_result?;
                            let payment_response = make_payment_response_header(true, None);

                            let (req, response) = res.into_parts();
                            let mut response = response.map_into_left_body();
                            if let Ok(hv) = HeaderValue::from_str(&payment_response) {
                                response.headers_mut().insert(
                                    HeaderName::from_static("payment-response"), hv,
                                );
                            }
                            Ok(ServiceResponse::new(req, response))
                        }
                        Ok(vr) => {
                            let error_msg = vr.invalid_reason.unwrap_or_else(|| "Payment verification failed".to_string());
                            warn!("Payment verification failed (parallel): {}", error_msg);
                            if let Some(ref payer) = payer_address {
                                verification_cache.record_failure(payer);
                            }

                            let payment_response = make_payment_response_header(false, Some(&error_msg));
                            let response = HttpResponse::PaymentRequired()
                                .insert_header((
                                    HeaderName::from_static("payment-response"),
                                    HeaderValue::from_str(&payment_response)
                                        .unwrap_or_else(|_| HeaderValue::from_static("")),
                                ))
                                .body(format!("Payment verification failed: {}", error_msg));

                            match service_result {
                                Ok(res) => {
                                    let (req, _) = res.into_parts();
                                    Ok(ServiceResponse::new(req, response.map_into_right_body()))
                                }
                                Err(_) => {
                                    Err(actix_web::error::ErrorPaymentRequired(error_msg))
                                }
                            }
                        }
                        Err(e) => {
                            error!("Facilitator error (parallel): {}", e);
                            let response = HttpResponse::BadGateway()
                                .body(format!("Facilitator error: {}", e));

                            match service_result {
                                Ok(res) => {
                                    let (req, _) = res.into_parts();
                                    Ok(ServiceResponse::new(req, response.map_into_right_body()))
                                }
                                Err(_) => {
                                    Err(actix_web::error::ErrorBadGateway(format!("Facilitator error: {}", e)))
                                }
                            }
                        }
                    }
                }
            }
        })
    }
}

/// Build a base64-encoded payment-response header value
fn make_payment_response_header(success: bool, error: Option<&str>) -> String {
    let response = serde_json::json!({
        "x402Version": 2,
        "success": success,
        "error": error,
    });
    let json = serde_json::to_string(&response).unwrap_or_default();
    base64::Engine::encode(&base64::engine::general_purpose::STANDARD, json.as_bytes())
}
