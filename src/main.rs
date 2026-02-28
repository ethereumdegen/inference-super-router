mod config;
mod endpoints;
mod erc8128;
mod error;
mod handler;
mod middleware;
mod models;
mod payment;
mod services;

use actix_cors::Cors;
use actix_files::Files;
use actix_web::{http::header, web, App, HttpResponse, HttpServer};
use config::{CreditsConfig, GlobalConfig};
use endpoints::{load_endpoints, resolve_endpoint, ResolvedEndpoint};
use middleware::{EndpointRegistry, RegisteredEndpoint, UnifiedDispatchMiddleware};
use payment::EndpointPaymentConfig;
use services::{
    CreditsClient, FacilitatorClient, InferenceClient, NonceTracker, RateLimiter, SessionManager,
    SettlementQueue, SettlementWorker, VerificationCache,
};
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

/// Root endpoint — returns human-readable plain text with all available models and usage info
async fn root_handler(
    registry: web::Data<EndpointRegistry>,
    global_config: web::Data<GlobalConfig>,
) -> HttpResponse {
    let mut out = String::new();
    out.push_str("inference-super-router\n");
    out.push_str(&format!("version: {}\n", env!("CARGO_PKG_VERSION")));
    out.push_str(&format!("wallet: {}\n", global_config.bot_wallet_address));
    out.push_str(&format!("test_mode: {}\n", global_config.test_mode));

    out.push_str("\n--- models ---\n\n");

    // Collect and sort models alphabetically
    let mut models: Vec<(&String, &RegisteredEndpoint)> = registry.models.iter().collect();
    models.sort_by_key(|(name, _)| name.to_lowercase());

    for (name, reg) in &models {
        out.push_str(&format!(
            "  {:<16} {:>5} {:<10} {}\n",
            name,
            reg.endpoint.def.cost,
            reg.endpoint.def.payment_currency,
            reg.endpoint.def.description,
        ));
    }

    // Show "auto" entry pointing to the default model
    if let Some(default_reg) = registry.models.get(&registry.default_model) {
        out.push_str(&format!(
            "  {:<16} {:>5} {:<10} {} (default)\n",
            "auto",
            default_reg.endpoint.def.cost,
            default_reg.endpoint.def.payment_currency,
            default_reg.endpoint.def.description,
        ));
    }

    out.push_str("\n--- usage ---\n\n");
    out.push_str("  POST /chat  {\"model\": \"auto\", \"messages\": [...]}\n");
    out.push_str("  POST /api/v1/chat/completions  (OpenAI-compatible)\n");
    out.push_str("\n  \"model\" is required. Use \"auto\" for the default (credits-enabled) model.\n");
    out.push_str("\n  \"payment_type\" is optional: \"auto\" (default), \"credits\", or \"x402\".\n");
    out.push_str("    auto    — try credits first (if ERC-8128 headers present), fall back to x402\n");
    out.push_str("    credits — only accept credits (requires ERC-8128 signed request)\n");
    out.push_str("    x402    — only accept x402 payment (requires X-PAYMENT header)\n");
    out.push_str("\n  https://www.x402.org\n");

    out.push_str("\n--- system routes ---\n\n");
    out.push_str("  GET /         this page\n");
    out.push_str("  GET /health   health check\n");
    out.push_str("  GET /metrics  settlement & cache metrics\n");

    HttpResponse::Ok()
        .content_type("text/plain")
        .body(out)
}

/// Credits balance endpoint — returns the caller's credit balance.
///
/// Accepts either a Bearer session token or ERC-8128 signed request headers.
async fn credits_balance_handler(
    req: actix_web::HttpRequest,
    credits_client: Option<web::Data<Arc<CreditsClient>>>,
    session_manager: web::Data<Arc<SessionManager>>,
) -> HttpResponse {
    let credits_client = match credits_client {
        Some(c) => c,
        None => {
            return HttpResponse::ServiceUnavailable().json(serde_json::json!({
                "error": "Credits system not enabled"
            }));
        }
    };

    // Try Bearer token first, fall back to ERC-8128
    let wallet = if let Some(token) = extract_bearer_token(req.headers()) {
        match session_manager.validate(&token) {
            Some(info) => info.wallet_address,
            None => {
                return HttpResponse::Unauthorized().json(serde_json::json!({
                    "error": "Invalid or expired session token"
                }));
            }
        }
    } else {
        match erc8128::verify_from_request(&req, &[]) {
            Ok(id) => id.wallet_address,
            Err(e) => {
                warn!("Credits balance: auth failed: {}", e);
                return HttpResponse::Unauthorized().json(serde_json::json!({
                    "error": format!("Bearer token or ERC-8128 signature required: {}", e)
                }));
            }
        }
    };

    match credits_client.get_credits(&wallet).await {
        Ok(balance) => {
            HttpResponse::Ok().json(serde_json::json!({
                "credits": balance,
                "address": wallet
            }))
        }
        Err(e) => {
            error!("Credits balance lookup failed for {}: {}", wallet, e);
            HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Failed to fetch credit balance"
            }))
        }
    }
}

/// Create a credits session — exchange one ERC-8128 signature for a Bearer token.
///
/// The client signs this single request with ERC-8128 headers, and receives
/// an opaque session token valid for ~1 hour, usable as `Authorization: Bearer <token>`.
async fn credits_session_handler(
    req: actix_web::HttpRequest,
    body: web::Bytes,
    credits_client: Option<web::Data<Arc<CreditsClient>>>,
    session_manager: web::Data<Arc<SessionManager>>,
) -> HttpResponse {
    let credits_client = match credits_client {
        Some(c) => c,
        None => {
            return HttpResponse::ServiceUnavailable().json(serde_json::json!({
                "error": "Credits system not enabled"
            }));
        }
    };

    // Verify ERC-8128 signature to recover wallet address
    let identity = match erc8128::verify_from_request(&req, &body) {
        Ok(id) => id,
        Err(e) => {
            warn!("Credits session: ERC-8128 verification failed: {}", e);
            return HttpResponse::Unauthorized().json(serde_json::json!({
                "error": format!("ERC-8128 signature required: {}", e)
            }));
        }
    };

    let wallet = identity.wallet_address.to_lowercase();

    // Sanity check: wallet has credits
    match credits_client.get_credits(&wallet).await {
        Ok(credits) if credits > 0 => {
            info!(
                "[SESSION] Creating session for wallet {} (credits: {})",
                wallet, credits
            );
        }
        Ok(credits) => {
            return HttpResponse::PaymentRequired().json(serde_json::json!({
                "error": format!("No credits available (balance: {})", credits),
                "credits": credits,
                "address": wallet
            }));
        }
        Err(e) => {
            error!("Credits check failed for session creation: {}", e);
            return HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "Failed to check credit balance"
            }));
        }
    }

    let (token, expires_at) = session_manager.create_session(&wallet, identity.chain_id);
    info!(
        "[SESSION] Session created for wallet {} (expires_at: {}, ttl: {}s)",
        wallet, expires_at, session_manager.ttl_secs()
    );

    HttpResponse::Ok().json(serde_json::json!({
        "session_token": token,
        "expires_at": expires_at,
        "wallet": wallet
    }))
}

/// Extract a Bearer token from the Authorization header.
fn extract_bearer_token(headers: &actix_web::http::header::HeaderMap) -> Option<String> {
    headers
        .get(actix_web::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|s| s.to_string())
}

/// Public endpoint catalog — returns JSON array of all available models with pricing.
/// Consumed by stark-bot (and any other client) to build UI dropdowns dynamically.
async fn endpoints_handler(
    registry: web::Data<EndpointRegistry>,
    global_config: web::Data<GlobalConfig>,
) -> HttpResponse {
    let base_url = global_config.base_url.as_deref().unwrap_or("https://inference.defirelay.com");
    let base_url = base_url.trim_end_matches('/');

    let mut entries: Vec<serde_json::Value> = registry
        .models
        .iter()
        .map(|(name, reg)| {
            let def = &reg.endpoint.def;
            let prefix = def.route_prefix.trim_end_matches('/');
            let endpoint_url = format!("{}{}/api/v1/chat/completions", base_url, prefix);

            serde_json::json!({
                "id": name,
                "display_name": def.description,
                "endpoint": endpoint_url,
                "model_archetype": def.archetype,
                "model": name,
                "x402_cost": def.cost.parse::<u64>().unwrap_or(0),
                "credit_cost": def.credit_cost,
                "max_input_tokens": def.max_input_tokens,
                "max_output_tokens": def.max_output_tokens,
            })
        })
        .collect();

    entries.sort_by(|a, b| {
        a["id"].as_str().unwrap_or("").cmp(b["id"].as_str().unwrap_or(""))
    });

    HttpResponse::Ok().json(entries)
}

/// Health check
async fn health_handler(
    settlement_queue: Option<web::Data<Arc<SettlementQueue>>>,
) -> HttpResponse {
    let queue_depth = settlement_queue
        .as_ref()
        .map(|q| q.len())
        .unwrap_or(0);

    HttpResponse::Ok().json(serde_json::json!({
        "status": "healthy",
        "service": "inference-super-router",
        "settlement_queue_depth": queue_depth
    }))
}

/// Metrics endpoint
async fn metrics_handler(
    settlement_queue: web::Data<Arc<SettlementQueue>>,
    worker_metrics: web::Data<Arc<services::SettlementMetrics>>,
    verification_cache: web::Data<Arc<VerificationCache>>,
) -> HttpResponse {
    let (total, success, failure, retries) = worker_metrics.get_stats();
    let (pending, in_progress, completed, failed) = settlement_queue.get_status_counts();
    let (cache_hits, cache_misses, cache_downgrades) = verification_cache.stats();

    HttpResponse::Ok().json(serde_json::json!({
        "settlement_queue": {
            "pending": pending,
            "in_progress": in_progress,
            "max_size": settlement_queue.max_size(),
            "is_full": settlement_queue.is_full()
        },
        "settlement_store": {
            "pending": pending,
            "in_progress": in_progress,
            "completed": completed,
            "failed": failed,
            "total": pending + in_progress + completed + failed
        },
        "settlement_worker": {
            "total_processed": total,
            "success_count": success,
            "failure_count": failure,
            "retry_count": retries
        },
        "verification_cache": {
            "hits": cache_hits,
            "misses": cache_misses,
            "downgrades": cache_downgrades,
            "hit_rate": if cache_hits + cache_misses > 0 {
                cache_hits as f64 / (cache_hits + cache_misses) as f64
            } else {
                0.0
            }
        }
    }))
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    // Load global config
    let global_config = match GlobalConfig::from_env() {
        Ok(c) => c,
        Err(e) => {
            error!("Configuration error: {}", e);
            std::process::exit(1);
        }
    };

    let port = global_config.port;
    info!("Starting inference-super-router on port {}", port);
    info!("Bot wallet: {}", global_config.bot_wallet_address);
    info!("Facilitator URL: {}", global_config.facilitator_url);
    info!("Test mode: {}", global_config.test_mode);

    // Initialize credits system (optional)
    let credits_client: Option<Arc<CreditsClient>> = match CreditsConfig::from_env() {
        Some(cfg) => {
            info!(
                "Credits system enabled (admin address: {})",
                cfg.signer.address()
            );
            let client = CreditsClient::new(&cfg.api_url, cfg.signer);
            Some(Arc::new(client))
        }
        None => {
            info!("Credits system disabled (CREDITS_ADMIN_PRIVATE_KEY not set)");
            None
        }
    };

    // Load and resolve endpoints from RON config
    let endpoints_config = load_endpoints(&global_config.endpoints_config_path);
    info!("Loaded {} endpoint definitions from {}", endpoints_config.endpoints.len(), global_config.endpoints_config_path);

    let mut resolved_endpoints: Vec<ResolvedEndpoint> = Vec::new();
    for def in endpoints_config.endpoints {
        match resolve_endpoint(def) {
            Ok(ep) => {
                info!(
                    "  {} -> {} [{}] cost={} ({}){}",
                    ep.def.name, ep.def.api_endpoint, ep.def.archetype,
                    ep.def.cost, ep.def.payment_currency,
                    if ep.def.credit_cost > 0 { format!(" [credits: {}]", ep.def.credit_cost) } else { String::new() }
                );
                resolved_endpoints.push(ep);
            }
            Err(e) => {
                error!("Failed to resolve endpoint: {}", e);
                std::process::exit(1);
            }
        }
    }

    // Build EndpointRegistry from resolved endpoints
    let mut registry_models: HashMap<String, RegisteredEndpoint> = HashMap::new();
    let mut default_model_name = String::new();

    for ep in &resolved_endpoints {
        let client = InferenceClient::new(
            &ep.def.api_endpoint,
            &ep.api_key,
            &ep.def.model,
            &ep.def.archetype,
        );

        let payment_config = EndpointPaymentConfig::from_config_and_endpoint(
            &global_config,
            &ep.def,
        );

        let credit_cost = if credits_client.is_some() { ep.def.credit_cost } else { 0 };

        registry_models.insert(ep.def.name.clone(), RegisteredEndpoint {
            endpoint: ep.clone(),
            client,
            payment_config,
            credit_cost,
        });

        if credit_cost > 0 && default_model_name.is_empty() {
            default_model_name = ep.def.name.clone();
        }
    }

    // Fallback: if no credits-enabled endpoint, use the first one as default
    if default_model_name.is_empty() && !registry_models.is_empty() {
        default_model_name = resolved_endpoints[0].def.name.clone();
    }

    let registry = Arc::new(EndpointRegistry {
        models: registry_models,
        default_model: default_model_name.clone(),
    });

    info!("EndpointRegistry built with {} models, default: '{}'", registry.models.len(), default_model_name);

    // Create shared services
    let facilitator_client = FacilitatorClient::new(&global_config.facilitator_url);
    let nonce_tracker = Arc::new(NonceTracker::with_default_ttl());
    let rate_limiter = Arc::new(RateLimiter::new(5));
    let verification_cache = Arc::new(VerificationCache::with_default_ttl());
    let session_manager = Arc::new(SessionManager::from_env());
    info!("Session manager initialized (TTL: {}s)", session_manager.ttl_secs());

    let max_queue_size = std::env::var("SETTLEMENT_QUEUE_MAX_SIZE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(services::DEFAULT_MAX_QUEUE_SIZE);
    let db_path = std::env::var("SETTLEMENT_DB_PATH")
        .unwrap_or_else(|_| "data/settlements.db".to_string());

    let settlement_queue = match SettlementQueue::with_store_and_max_size(&db_path, max_queue_size) {
        Ok(q) => Arc::new(q),
        Err(e) => {
            error!("Failed to initialize settlement store at {}: {}", db_path, e);
            std::process::exit(1);
        }
    };
    info!("Settlement queue initialized (SQLite at {}, max size: {})", db_path, max_queue_size);

    // Spawn background settlement worker
    let (shutdown_tx, _) = tokio::sync::broadcast::channel::<()>(1);
    let worker = SettlementWorker::new(settlement_queue.clone(), facilitator_client.clone());
    let worker_metrics = worker.metrics();
    let shutdown_rx = shutdown_tx.subscribe();
    tokio::spawn(async move {
        worker.run(shutdown_rx).await;
    });
    info!("Background settlement worker started");

    // Keep clones for shutdown logic (after server stops)
    let settlement_queue_for_shutdown = settlement_queue.clone();
    let worker_metrics_for_shutdown = worker_metrics.clone();

    // Prepare data for App factory closure
    let settlement_queue_for_app = settlement_queue.clone();
    let worker_metrics_for_app = worker_metrics.clone();
    let verification_cache_for_app = verification_cache.clone();
    let credits_client_for_app = credits_client.clone();
    let session_manager_for_app = session_manager.clone();
    let shutdown_tx_clone = shutdown_tx.clone();
    let registry_for_app = registry.clone();

    let server = HttpServer::new(move || {
        let cors = Cors::default()
            .allow_any_origin()
            .allowed_methods(vec!["GET", "POST", "OPTIONS"])
            .allowed_headers(vec![
                header::CONTENT_TYPE,
                header::AUTHORIZATION,
                header::HeaderName::from_static("x-payment"),
                header::HeaderName::from_static("signature-input"),
                header::HeaderName::from_static("signature"),
                header::HeaderName::from_static("content-digest"),
            ])
            .expose_headers(vec![
                header::HeaderName::from_static("payment-required"),
                header::HeaderName::from_static("payment-response"),
                header::HeaderName::from_static("x-erc8128-credits"),
            ])
            .max_age(3600);

        let chat_scope = web::scope("/chat")
            .wrap(UnifiedDispatchMiddleware::new(
                global_config.clone(),
                registry_for_app.clone(),
                facilitator_client.clone(),
                nonce_tracker.clone(),
                settlement_queue.clone(),
                rate_limiter.clone(),
                verification_cache.clone(),
                credits_client_for_app.clone(),
                session_manager_for_app.clone(),
            ))
            .route("", web::post().to(handler::unified_chat_handler));

        let api_scope = web::scope("/api/v1/chat")
            .wrap(UnifiedDispatchMiddleware::new(
                global_config.clone(),
                registry_for_app.clone(),
                facilitator_client.clone(),
                nonce_tracker.clone(),
                settlement_queue.clone(),
                rate_limiter.clone(),
                verification_cache.clone(),
                credits_client_for_app.clone(),
                session_manager_for_app.clone(),
            ))
            .route("/completions", web::post().to(handler::unified_chat_handler));

        let mut app = App::new()
            .wrap(cors)
            .app_data(web::Data::new(global_config.clone()))
            .app_data(web::Data::from(registry_for_app.clone()))
            .app_data(web::Data::new(settlement_queue_for_app.clone()))
            .app_data(web::Data::new(worker_metrics_for_app.clone()))
            .app_data(web::Data::new(verification_cache_for_app.clone()))
            .app_data(web::Data::new(session_manager_for_app.clone()));

        // Conditionally register credits_client as app_data (for balance endpoint)
        if let Some(ref cc) = credits_client_for_app {
            app = app.app_data(web::Data::new(cc.clone()));
        }

        app
            // Public endpoints
            .route("/", web::get().to(root_handler))
            .route("/endpoints", web::get().to(endpoints_handler))
            .route("/health", web::get().to(health_handler))
            .route("/metrics", web::get().to(metrics_handler))
            .route("/credits/balance", web::get().to(credits_balance_handler))
            .route("/credits/session", web::post().to(credits_session_handler))
            .service(Files::new("/.well-known", "public/.well-known"))
            .service(chat_scope)
            .service(api_scope)
    })
    .bind(("0.0.0.0", port))?
    .run();

    let result = server.await;

    info!("Server stopping, signaling settlement worker to shut down...");
    let _ = shutdown_tx_clone.send(());
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let remaining = settlement_queue_for_shutdown.len();
    if remaining > 0 {
        info!(
            "Shutting down with {} pending settlements (persisted to SQLite)",
            remaining
        );
    }

    let (total, success, failure, retries) = worker_metrics_for_shutdown.get_stats();
    info!(
        "Final settlement stats: total={}, success={}, failure={}, retries={}",
        total, success, failure, retries
    );

    result
}
