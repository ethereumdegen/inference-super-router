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
        let route = "/admin/credits";
        let query = format!("address={}", wallet_address);
        let url = format!("{}{}?{}", self.base_url, route, query);

        // Sign with the full path as seen by the server (path_prefix + route)
        let authority = extract_authority(&self.base_url);
        let signing_path = format!("{}{}", extract_path_prefix(&self.base_url), route);

        debug!(
            "[CREDITS_CLIENT] get_credits: url={}, authority={}, signing_path={}, signer_addr={}",
            url, authority, signing_path, self.signer.address()
        );

        let signed = self
            .signer
            .sign_request("GET", &authority, &signing_path, Some(&query), None)
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
            error!("[CREDITS_CLIENT] get_credits failed: status={}, body={}, url={}", status, body, url);
            return Err(format!("Credits API error {}: {}", status, body));
        }

        debug!("[CREDITS_CLIENT] get_credits response: {}", body);

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
        let route = "/admin/credits";
        let url = format!("{}{}", self.base_url, route);

        let body_json = serde_json::json!({
            "address": wallet_address,
            "delta": delta,
        });
        let body_bytes = serde_json::to_vec(&body_json)
            .map_err(|e| format!("Failed to serialize credits body: {}", e))?;

        // Sign with the full path as seen by the server (path_prefix + route)
        let authority = extract_authority(&self.base_url);
        let signing_path = format!("{}{}", extract_path_prefix(&self.base_url), route);

        debug!(
            "[CREDITS_CLIENT] adjust_credits: url={}, authority={}, signing_path={}, wallet={}, delta={}",
            url, authority, signing_path, wallet_address, delta
        );

        let signed = self
            .signer
            .sign_request("POST", &authority, &signing_path, None, Some(&body_bytes))
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
            error!("[CREDITS_CLIENT] adjust_credits failed: status={}, body={}, url={}", status, resp_body, url);
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

/// Extract the path prefix from a base URL.
/// e.g. "https://starkbot.cloud/api" → "/api"
/// e.g. "https://starkbot.cloud" → ""
fn extract_path_prefix(url: &str) -> String {
    let without_scheme = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);

    match without_scheme.find('/') {
        Some(idx) => without_scheme[idx..].trim_end_matches('/').to_string(),
        None => String::new(),
    }
}
