//! HTTP client for the starkbot.cloud credits API.
//!
//! All requests to the credits API are authenticated with a shared secret key
//! sent as `Authorization: Bearer <secret>`.

use reqwest::Client;
use tracing::{debug, error};

/// Client for the starkbot.cloud `/admin/credits` API.
#[derive(Clone)]
pub struct CreditsClient {
    http: Client,
    base_url: String,
    secret_key: String,
}

impl CreditsClient {
    pub fn new(base_url: &str, secret_key: String) -> Self {
        Self {
            http: Client::new(),
            base_url: base_url.trim_end_matches('/').to_string(),
            secret_key,
        }
    }

    /// Get credit balance for a wallet address.
    /// Returns the number of credits, or an error.
    pub async fn get_credits(&self, wallet_address: &str) -> Result<i64, String> {
        let route = "/admin/credits";
        let query = format!("address={}", wallet_address);
        let url = format!("{}{}?{}", self.base_url, route, query);

        debug!("[CREDITS_CLIENT] get_credits: url={}", url);

        let resp = self
            .http
            .get(&url)
            .header("authorization", format!("Bearer {}", self.secret_key))
            .send()
            .await
            .map_err(|e| format!("Credits API request failed: {}", e))?;

        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| format!("Failed to read credits response: {}", e))?;

        if !status.is_success() {
            error!(
                "[CREDITS_CLIENT] get_credits failed: status={}, body={}, url={}",
                status, body, url
            );
            return Err(format!("Credits API error {}: {}", status, body));
        }

        debug!("[CREDITS_CLIENT] get_credits response: {}", body);

        let json: serde_json::Value = serde_json::from_str(&body).map_err(|e| {
            error!(
                "[CREDITS_CLIENT] Invalid JSON from credits API: error={}, url={}, body_preview={}",
                e,
                url,
                &body[..body.len().min(200)]
            );
            format!("Invalid credits JSON: {}", e)
        })?;

        let credits = json
            .get("credits")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| format!("Missing 'credits' field in response: {}", body))?;

        debug!("Credits for {}: {}", wallet_address, credits);
        Ok(credits)
    }

    /// Adjust credits for a wallet address by a delta (negative to deduct).
    /// Returns the new balance.
    pub async fn adjust_credits(
        &self,
        wallet_address: &str,
        delta: i64,
    ) -> Result<i64, String> {
        let route = "/admin/credits";
        let url = format!("{}{}", self.base_url, route);

        let body_json = serde_json::json!({
            "address": wallet_address,
            "delta": delta,
        });

        debug!(
            "[CREDITS_CLIENT] adjust_credits: url={}, wallet={}, delta={}",
            url, wallet_address, delta
        );

        let resp = self
            .http
            .post(&url)
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", self.secret_key))
            .json(&body_json)
            .send()
            .await
            .map_err(|e| format!("Credits API request failed: {}", e))?;

        let status = resp.status();
        let resp_body = resp
            .text()
            .await
            .map_err(|e| format!("Failed to read credits response: {}", e))?;

        if !status.is_success() {
            error!(
                "[CREDITS_CLIENT] adjust_credits failed: status={}, body={}, url={}",
                status, resp_body, url
            );
            return Err(format!("Credits API error {}: {}", status, resp_body));
        }

        let json: serde_json::Value = serde_json::from_str(&resp_body)
            .map_err(|e| format!("Invalid credits JSON: {}", e))?;

        let new_balance = json
            .get("credits")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| format!("Missing 'credits' field in response: {}", resp_body))?;

        debug!(
            "Adjusted credits for {} by {}: new balance {}",
            wallet_address, delta, new_balance
        );
        Ok(new_balance)
    }
}
