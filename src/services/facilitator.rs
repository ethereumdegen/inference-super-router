use crate::error::AppError;
use backoff::{backoff::Backoff, ExponentialBackoff};
use reqwest::Client;
use std::time::Duration;
use tracing::{debug, error, info, warn};

const MAX_RETRIES: u32 = 3;

#[derive(Clone)]
pub struct FacilitatorConfig {
    pub max_retries: u32,
    pub initial_interval: Duration,
    pub max_interval: Duration,
    pub request_timeout: Duration,
}

impl Default for FacilitatorConfig {
    fn default() -> Self {
        Self {
            max_retries: MAX_RETRIES,
            initial_interval: Duration::from_millis(100),
            max_interval: Duration::from_secs(2),
            request_timeout: Duration::from_secs(30),
        }
    }
}

/// Protocol-agnostic facilitator client.
/// Accepts raw JSON for verify/settle requests (works with both x402 v1 and v2).
#[derive(Clone)]
pub struct FacilitatorClient {
    client: Client,
    base_url: String,
    config: FacilitatorConfig,
}

impl FacilitatorClient {
    pub fn new(base_url: &str) -> Self {
        Self::with_config(base_url, FacilitatorConfig::default())
    }

    pub fn with_config(base_url: &str, config: FacilitatorConfig) -> Self {
        let client = Client::builder()
            .timeout(config.request_timeout)
            .build()
            .expect("Failed to build HTTP client");

        FacilitatorClient {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            config,
        }
    }

    fn create_backoff(&self) -> ExponentialBackoff {
        ExponentialBackoff {
            initial_interval: self.config.initial_interval,
            max_interval: self.config.max_interval,
            max_elapsed_time: Some(Duration::from_secs(10)),
            ..ExponentialBackoff::default()
        }
    }

    fn is_transient_error(err: &reqwest::Error) -> bool {
        err.is_timeout() || err.is_connect() || err.is_request()
    }

    fn is_transient_status(status: reqwest::StatusCode) -> bool {
        status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS
    }

    /// Verify a payment with the facilitator (protocol-agnostic, accepts raw JSON).
    /// Returns the raw JSON response from the facilitator.
    pub async fn verify_raw(&self, request_json: &serde_json::Value) -> Result<VerifyResult, AppError> {
        let url = format!("{}/verify", self.base_url);
        debug!("Sending verify request to facilitator: {}", url);

        let mut backoff = self.create_backoff();
        let mut attempts = 0;
        let mut last_error: Option<AppError> = None;

        loop {
            attempts += 1;

            let result = self.client.post(&url).json(request_json).send().await;

            match result {
                Ok(response) => {
                    if response.status().is_success() {
                        let body: serde_json::Value = response.json().await.map_err(|e| {
                            error!("Failed to parse facilitator response: {}", e);
                            AppError::Facilitator(format!("Invalid response: {}", e))
                        })?;

                        let is_valid = body.get("isValid").and_then(|v| v.as_bool()).unwrap_or(false);
                        let payer = body.get("payer").and_then(|v| v.as_str()).map(|s| s.to_string());
                        let invalid_reason = body.get("invalidReason").and_then(|v| v.as_str()).map(|s| s.to_string());

                        if is_valid {
                            info!("Payment verified for payer: {:?}", payer);
                        } else {
                            info!("Payment rejected: {:?}", invalid_reason);
                        }

                        return Ok(VerifyResult { is_valid, payer, invalid_reason });
                    }

                    let status = response.status();
                    let body = response.text().await.unwrap_or_default();

                    if Self::is_transient_status(status) && attempts <= self.config.max_retries {
                        warn!("Facilitator returned transient error: {} - {} (attempt {}/{})", status, body, attempts, self.config.max_retries);
                        last_error = Some(AppError::Facilitator(format!("Facilitator returned {}: {}", status, body)));
                        if let Some(duration) = backoff.next_backoff() {
                            tokio::time::sleep(duration).await;
                            continue;
                        }
                    }

                    error!("Facilitator returned error: {} - {}", status, body);
                    return Err(AppError::Facilitator(format!("Facilitator returned {}: {}", status, body)));
                }
                Err(e) => {
                    if Self::is_transient_error(&e) && attempts <= self.config.max_retries {
                        warn!("Transient error connecting to facilitator: {} (attempt {}/{})", e, attempts, self.config.max_retries);
                        last_error = Some(AppError::Facilitator(format!("Connection failed: {}", e)));
                        if let Some(duration) = backoff.next_backoff() {
                            tokio::time::sleep(duration).await;
                            continue;
                        }
                    }

                    error!("Failed to connect to facilitator: {}", e);
                    return Err(last_error.unwrap_or_else(|| AppError::Facilitator(format!("Connection failed: {}", e))));
                }
            }
        }
    }

    /// Settle a payment with the facilitator (protocol-agnostic, accepts raw JSON).
    /// Returns the raw JSON response.
    pub async fn settle_raw(&self, request_json: &serde_json::Value) -> Result<SettleResult, AppError> {
        let url = format!("{}/settle", self.base_url);
        debug!("Sending settle request to facilitator: {}", url);

        let mut backoff = self.create_backoff();
        let mut attempts = 0;
        let mut last_error: Option<AppError> = None;

        loop {
            attempts += 1;

            let result = self.client.post(&url).json(request_json).send().await;

            match result {
                Ok(response) => {
                    if response.status().is_success() {
                        let body: serde_json::Value = response.json().await.map_err(|e| {
                            error!("Failed to parse facilitator settlement response: {}", e);
                            AppError::Facilitator(format!("Invalid settlement response: {}", e))
                        })?;

                        let success = body.get("success").and_then(|v| v.as_bool()).unwrap_or(false);
                        let transaction = body.get("transaction").and_then(|v| v.as_str()).map(|s| s.to_string());
                        let payer = body.get("payer").and_then(|v| v.as_str()).map(|s| s.to_string());
                        let error_reason = body.get("errorReason").and_then(|v| v.as_str()).map(|s| s.to_string());

                        if success {
                            info!("Payment settled successfully. Tx: {:?}, Payer: {:?}", transaction, payer);
                        } else {
                            warn!("Payment settlement failed: {:?}", error_reason);
                        }

                        return Ok(SettleResult { success, transaction, payer, error_reason });
                    }

                    let status = response.status();
                    let body = response.text().await.unwrap_or_default();

                    if Self::is_transient_status(status) && attempts <= self.config.max_retries {
                        warn!("Facilitator settlement returned transient error: {} - {} (attempt {}/{})", status, body, attempts, self.config.max_retries);
                        last_error = Some(AppError::Facilitator(format!("Settlement failed with {}: {}", status, body)));
                        if let Some(duration) = backoff.next_backoff() {
                            tokio::time::sleep(duration).await;
                            continue;
                        }
                    }

                    error!("Facilitator settlement returned error: {} - {}", status, body);
                    return Err(AppError::Facilitator(format!("Settlement failed with {}: {}", status, body)));
                }
                Err(e) => {
                    if Self::is_transient_error(&e) && attempts <= self.config.max_retries {
                        warn!("Transient error connecting to facilitator for settlement: {} (attempt {}/{})", e, attempts, self.config.max_retries);
                        last_error = Some(AppError::Facilitator(format!("Settlement connection failed: {}", e)));
                        if let Some(duration) = backoff.next_backoff() {
                            tokio::time::sleep(duration).await;
                            continue;
                        }
                    }

                    error!("Failed to connect to facilitator for settlement: {}", e);
                    return Err(last_error.unwrap_or_else(|| AppError::Facilitator(format!("Settlement connection failed: {}", e))));
                }
            }
        }
    }
}

/// Result of a verify call
#[derive(Debug, Clone)]
pub struct VerifyResult {
    pub is_valid: bool,
    pub payer: Option<String>,
    pub invalid_reason: Option<String>,
}

/// Result of a settle call
#[derive(Debug, Clone)]
pub struct SettleResult {
    pub success: bool,
    pub transaction: Option<String>,
    pub payer: Option<String>,
    pub error_reason: Option<String>,
}
