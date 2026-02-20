//! Credits-or-x402 middleware.
//!
//! If the request carries ERC-8128 headers, verify the signature, check the
//! caller's credit balance on starkbot.cloud, deduct 1 credit, and serve.
//! Otherwise, fall through to the standard x402 payment flow.
//!
//! When falling through to x402, the 402 response includes an
//! `x-erc8128-credits: true` header so clients can discover the credits option.

use super::x402::handle_x402_payment;
use crate::config::GlobalConfig;
use crate::erc8128;
use crate::payment::EndpointPaymentConfig;
use crate::services::{
    CreditsClient, FacilitatorClient, NonceTracker, RateLimiter, SettlementQueue,
    VerificationCache,
};
use actix_web::{
    body::EitherBody,
    dev::{forward_ready, Service, ServiceRequest, ServiceResponse, Transform},
    web::{Bytes, BytesMut},
    Error, HttpMessage,
};
use futures_core::Stream;
use std::future::{poll_fn, ready, Future, Ready};
use std::pin::Pin;
use std::sync::Arc;
use tracing::{debug, info, warn};

pub struct CreditsOrX402Middleware {
    global_config: GlobalConfig,
    payment_config: EndpointPaymentConfig,
    facilitator: FacilitatorClient,
    nonce_tracker: Arc<NonceTracker>,
    settlement_queue: Arc<SettlementQueue>,
    rate_limiter: Arc<RateLimiter>,
    verification_cache: Arc<VerificationCache>,
    credits_client: Arc<CreditsClient>,
    credit_cost: i64,
}

impl CreditsOrX402Middleware {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        global_config: GlobalConfig,
        payment_config: EndpointPaymentConfig,
        facilitator: FacilitatorClient,
        nonce_tracker: Arc<NonceTracker>,
        settlement_queue: Arc<SettlementQueue>,
        rate_limiter: Arc<RateLimiter>,
        verification_cache: Arc<VerificationCache>,
        credits_client: Arc<CreditsClient>,
        credit_cost: i64,
    ) -> Self {
        Self {
            global_config,
            payment_config,
            facilitator,
            nonce_tracker,
            settlement_queue,
            rate_limiter,
            verification_cache,
            credits_client,
            credit_cost,
        }
    }
}

impl<S, B> Transform<S, ServiceRequest> for CreditsOrX402Middleware
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    S::Future: 'static,
    B: 'static,
{
    type Response = ServiceResponse<EitherBody<B>>;
    type Error = Error;
    type InitError = ();
    type Transform = CreditsOrX402MiddlewareService<S>;
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        ready(Ok(CreditsOrX402MiddlewareService {
            service: Arc::new(service),
            global_config: self.global_config.clone(),
            payment_config: self.payment_config.clone(),
            facilitator: self.facilitator.clone(),
            nonce_tracker: self.nonce_tracker.clone(),
            settlement_queue: self.settlement_queue.clone(),
            rate_limiter: self.rate_limiter.clone(),
            verification_cache: self.verification_cache.clone(),
            credits_client: self.credits_client.clone(),
            credit_cost: self.credit_cost,
        }))
    }
}

pub struct CreditsOrX402MiddlewareService<S> {
    service: Arc<S>,
    global_config: GlobalConfig,
    payment_config: EndpointPaymentConfig,
    facilitator: FacilitatorClient,
    nonce_tracker: Arc<NonceTracker>,
    settlement_queue: Arc<SettlementQueue>,
    rate_limiter: Arc<RateLimiter>,
    verification_cache: Arc<VerificationCache>,
    credits_client: Arc<CreditsClient>,
    credit_cost: i64,
}

impl<S, B> Service<ServiceRequest> for CreditsOrX402MiddlewareService<S>
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
        let credits_client = self.credits_client.clone();
        let credit_cost = self.credit_cost;

        Box::pin(async move {
            // TEST_MODE: skip all payment/credits
            if global_config.test_mode {
                debug!("TEST_MODE: skipping credits/x402");
                let res = service.call(req).await?;
                return Ok(res.map_into_left_body());
            }

            let mut req = req;

            // ── Try ERC-8128 credits path ──
            if erc8128::has_erc8128_headers(req.headers()) {
                debug!("ERC-8128 headers detected, attempting credits path");

                // Buffer the request body for signature verification
                let body_bytes = drain_payload(&mut req).await;

                // Verify ERC-8128 signature
                match erc8128::verify_from_request(req.request(), &body_bytes) {
                    Ok(identity) => {
                        let wallet = identity.wallet_address.to_lowercase();
                        info!("ERC-8128 verified for wallet: {}", wallet);

                        // Check credit balance
                        match credits_client.get_credits(&wallet).await {
                            Ok(credits) if credits >= credit_cost => {
                                // Deduct credits
                                match credits_client.adjust_credits(&wallet, -credit_cost).await {
                                    Ok(new_balance) => {
                                        info!(
                                            "Deducted {} credits from {}: {} remaining",
                                            credit_cost, wallet, new_balance
                                        );

                                        // Re-attach body and call service
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

                        // Re-attach body before falling through to x402
                        set_payload_from_bytes(&mut req, body_bytes);
                    }
                    Err(e) => {
                        warn!("ERC-8128 verification failed: {}", e);
                        set_payload_from_bytes(&mut req, body_bytes);
                    }
                }
            }

            // ── Fall through to x402 ──
            // Advertise ERC-8128 credits support in 402 responses
            let extra_headers: &[(&str, &str)] = &[("x-erc8128-credits", "true")];

            handle_x402_payment(
                req,
                service,
                &global_config,
                &payment_config,
                &facilitator,
                &nonce_tracker,
                &settlement_queue,
                &rate_limiter,
                &verification_cache,
                Some(extra_headers),
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
        let chunk =
            poll_fn(|cx| Pin::new(&mut payload).poll_next(cx)).await;
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
