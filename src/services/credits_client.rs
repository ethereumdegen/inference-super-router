//! HTTP client for the starkbot.cloud credits API.
//!
//! All requests to the credits API are signed with an admin ERC-8128 key.

use crate::erc8128::Erc8128Signer;
use reqwest::Client;
use tracing::{debug, error};

/// Client for the starkbot.cloud `/admin/credits` API.
#[derive(Clone)]
pub struct CreditsClient {
    http: Client,
    base_url: String,
    signer: Erc8128Signer,
}

impl CreditsClient {
    pub fn new(base_url: &str, signer: Erc8128Signer) -> Self {
        Self {
            http: Client::new(),
            base_url: base_url.trim_end_matches('/').to_string(),
            signer,
        }
    }

    /// Get credit balance for a wallet address.
    /// Returns the number of credits, or an error.
    pub async fn get_credits(&self, wallet_address: &str) -> Result<i64, String> {
        let path = "/admin/credits";
        let query = format!("address={}", wallet_address);
        let url = format!("{}{}?{}", self.base_url, path, query);

        // Parse authority from base_url
        let authority = extract_authority(&self.base_url);

        let signed = self
            .signer
            .sign_request("GET", &authority, path, Some(&query), None)
            .map_err(|e| format!("Failed to sign credits request: {}", e))?;

        let mut req = self.http.get(&url);
        req = req.header("signature-input", &signed.signature_input);
        req = req.header("signature", &signed.signature);
        if let Some(ref digest) = signed.content_digest {
            req = req.header("content-digest", digest);
        }

        let resp = req
            .send()
            .await
            .map_err(|e| format!("Credits API request failed: {}", e))?;

        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| format!("Failed to read credits response: {}", e))?;

        if !status.is_success() {
            error!("Credits API returned {}: {}", status, body);
            return Err(format!("Credits API error {}: {}", status, body));
        }

        // Parse response: expect {"credits": N} or just a number
        let json: serde_json::Value = serde_json::from_str(&body)
            .map_err(|e| format!("Invalid credits JSON: {}", e))?;

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
        let path = "/admin/credits";
        let url = format!("{}{}", self.base_url, path);

        let body_json = serde_json::json!({
            "address": wallet_address,
            "delta": delta,
        });
        let body_bytes = serde_json::to_vec(&body_json)
            .map_err(|e| format!("Failed to serialize credits body: {}", e))?;

        let authority = extract_authority(&self.base_url);

        let signed = self
            .signer
            .sign_request("POST", &authority, path, None, Some(&body_bytes))
            .map_err(|e| format!("Failed to sign credits request: {}", e))?;

        let mut req = self
            .http
            .post(&url)
            .header("content-type", "application/json")
            .header("signature-input", &signed.signature_input)
            .header("signature", &signed.signature);

        if let Some(ref digest) = signed.content_digest {
            req = req.header("content-digest", digest);
        }

        let resp = req
            .body(body_bytes)
            .send()
            .await
            .map_err(|e| format!("Credits API request failed: {}", e))?;

        let status = resp.status();
        let resp_body = resp
            .text()
            .await
            .map_err(|e| format!("Failed to read credits response: {}", e))?;

        if !status.is_success() {
            error!("Credits API returned {}: {}", status, resp_body);
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

/// Extract authority (host:port or host) from a URL string.
fn extract_authority(url: &str) -> String {
    url.strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url)
        .split('/')
        .next()
        .unwrap_or("")
        .to_string()
}
