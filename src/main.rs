mod config;
mod endpoints;
mod error;
mod handler;
mod middleware;
mod models;
mod payment;
mod services;

use actix_cors::Cors;
use actix_files::Files;
use actix_web::{http::header, web, App, HttpResponse, HttpServer};
use config::GlobalConfig;
use endpoints::{load_endpoints, resolve_endpoint, ResolvedEndpoint};
use middleware::X402Middleware;
use payment::EndpointPaymentConfig;
use services::{
    FacilitatorClient, InferenceClient, NonceTracker, RateLimiter, SettlementQueue,
    SettlementWorker, VerificationCache,
};
use std::sync::Arc;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

/// Root endpoint — returns JSON with all available endpoints and usage info
async fn root_handler(
    endpoints_info: web::Data<Vec<EndpointInfo>>,
) -> HttpResponse {
    let endpoints_json: Vec<serde_json::Value> = endpoints_info
        .iter()
        .map(|ep| {
            serde_json::json!({
                "name": ep.name,
                "model": ep.model,
                "description": ep.description,
                "routes": {
                    "chat": format!("{}/chat", ep.route_prefix),
                    "openai_compatible": format!("{}/api/v1/chat/completions", ep.route_prefix),
                },
                "payment": {
                    "currency": ep.payment_currency,
                    "cost": ep.cost,
                },
                "limits": {
                    "max_input_tokens": ep.max_input_tokens,
                    "max_output_tokens": ep.max_output_tokens,
                },
            })
        })
        .collect();

    HttpResponse::Ok().json(serde_json::json!({
        "service": "inference-super-router",
        "description": "Multiplexed AI inference via x402 payment protocol",
        "endpoints": endpoints_json,
        "usage": {
            "step_1": "POST to any endpoint route with an OpenAI-compatible chat payload",
            "step_2": "If no payment header is present, you receive a 402 with payment requirements",
            "step_3": "Create a payment using the x402 protocol and include it in the X-PAYMENT header (base64)",
            "step_4": "The router verifies payment and proxies your request to the AI backend",
        },
        "system_routes": {
            "info": "/",
            "health": "/health",
            "metrics": "/metrics",
        },
        "x402": "https://www.x402.org",
    }))
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

/// Info stored about each endpoint for the root handler
#[derive(Clone)]
struct EndpointInfo {
    name: String,
    route_prefix: String,
    model: String,
    description: String,
    cost: String,
    payment_currency: String,
    max_input_tokens: u32,
    max_output_tokens: u32,
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

    // Load and resolve endpoints from RON config
    let endpoints_config = load_endpoints(&global_config.endpoints_config_path);
    info!("Loaded {} endpoint definitions from {}", endpoints_config.endpoints.len(), global_config.endpoints_config_path);

    let mut resolved_endpoints: Vec<ResolvedEndpoint> = Vec::new();
    for def in endpoints_config.endpoints {
        match resolve_endpoint(def) {
            Ok(ep) => {
                info!(
                    "  {} -> {} [{}] cost={} ({})",
                    ep.def.route_prefix, ep.def.api_endpoint, ep.def.archetype,
                    ep.def.cost, ep.def.payment_currency
                );
                resolved_endpoints.push(ep);
            }
            Err(e) => {
                error!("Failed to resolve endpoint: {}", e);
                std::process::exit(1);
            }
        }
    }

    // Build endpoint info for root handler
    let endpoints_info: Vec<EndpointInfo> = resolved_endpoints
        .iter()
        .map(|ep| EndpointInfo {
            name: ep.def.name.clone(),
            route_prefix: ep.def.route_prefix.clone(),
            model: ep.def.model.clone(),
            description: ep.def.description.clone(),
            cost: ep.def.cost.clone(),
            payment_currency: ep.def.payment_currency.clone(),
            max_input_tokens: ep.def.max_input_tokens,
            max_output_tokens: ep.def.max_output_tokens,
        })
        .collect();

    // Create shared services
    let facilitator_client = FacilitatorClient::new(&global_config.facilitator_url);
    let nonce_tracker = Arc::new(NonceTracker::with_default_ttl());
    let rate_limiter = Arc::new(RateLimiter::new(5));
    let verification_cache = Arc::new(VerificationCache::with_default_ttl());

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
    let shutdown_tx_clone = shutdown_tx.clone();

    let server = HttpServer::new(move || {
        let cors = Cors::default()
            .allow_any_origin()
            .allowed_methods(vec!["GET", "POST", "OPTIONS"])
            .allowed_headers(vec![
                header::CONTENT_TYPE,
                header::AUTHORIZATION,
                header::HeaderName::from_static("x-payment"),
            ])
            .expose_headers(vec![
                header::HeaderName::from_static("payment-required"),
                header::HeaderName::from_static("payment-response"),
            ])
            .max_age(3600);

        let mut app = App::new()
            .wrap(cors)
            .app_data(web::Data::new(endpoints_info.clone()))
            .app_data(web::Data::new(settlement_queue_for_app.clone()))
            .app_data(web::Data::new(worker_metrics_for_app.clone()))
            .app_data(web::Data::new(verification_cache_for_app.clone()))
            // Public endpoints
            .route("/", web::get().to(root_handler))
            .route("/health", web::get().to(health_handler))
            .route("/metrics", web::get().to(metrics_handler))
            .service(Files::new("/.well-known", "public/.well-known"));

        // Dynamic route registration from resolved endpoints
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

            let ep_clone = ep.clone();
            let prefix = ep.def.route_prefix.clone();

            // Register {prefix}/chat
            let chat_scope = web::scope(&format!("{}/chat", prefix))
                .wrap(X402Middleware::new(
                    global_config.clone(),
                    payment_config.clone(),
                    facilitator_client.clone(),
                    nonce_tracker.clone(),
                    settlement_queue.clone(),
                    rate_limiter.clone(),
                    verification_cache.clone(),
                ))
                .app_data(web::Data::new(client.clone()))
                .app_data(web::Data::new(ep_clone.clone()))
                .route("", web::post().to(handler::chat_handler));

            // Register {prefix}/api/v1/chat/completions
            let api_scope = web::scope(&format!("{}/api/v1/chat", prefix))
                .wrap(X402Middleware::new(
                    global_config.clone(),
                    payment_config,
                    facilitator_client.clone(),
                    nonce_tracker.clone(),
                    settlement_queue.clone(),
                    rate_limiter.clone(),
                    verification_cache.clone(),
                ))
                .app_data(web::Data::new(client))
                .app_data(web::Data::new(ep_clone))
                .route("/completions", web::post().to(handler::chat_handler));

            app = app.service(chat_scope).service(api_scope);
        }

        app
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
