use crate::config::GlobalConfig;
use crate::endpoints::EndpointDef;
use crate::models::{DomainEthAddress, DomainUint256};

/// Payment currency configuration for an endpoint
#[derive(Debug, Clone)]
pub enum PaymentCurrency {
    /// x402 v2 — "exact" scheme, CAIP-2 network IDs, USDC
    Usdc {
        network: String,
        asset: DomainEthAddress,
    },
    /// x402 v1 — "permit" scheme, simple network names, Starkbot
    Starkbot {
        network: String,
        asset: String,
        symbol: String,
        decimals: u8,
        name: String,
        version: String,
        facilitator_signer: String,
    },
}

/// Resolved payment config for a specific endpoint
#[derive(Debug, Clone)]
pub struct EndpointPaymentConfig {
    pub currency: PaymentCurrency,
    pub cost: String,
    pub pay_to: DomainEthAddress,
    pub description: String,
}

impl EndpointPaymentConfig {
    /// Build from global config + endpoint definition
    pub fn from_config_and_endpoint(config: &GlobalConfig, endpoint: &EndpointDef) -> Self {
        let currency = match endpoint.payment_currency.as_str() {
            "starkbot" => PaymentCurrency::Starkbot {
                network: config.starkbot_network.clone(),
                asset: config.starkbot_token_address.clone(),
                symbol: config.starkbot_token_symbol.clone(),
                decimals: config.starkbot_token_decimals,
                name: config.starkbot_token_name.clone(),
                version: config.starkbot_token_version.clone(),
                facilitator_signer: config.facilitator_signer.clone(),
            },
            _ => PaymentCurrency::Usdc {
                network: config.usdc_network.clone(),
                asset: config.usdc_token_address,
            },
        };

        EndpointPaymentConfig {
            currency,
            cost: endpoint.cost.clone(),
            pay_to: config.bot_wallet_address,
            description: endpoint.description.clone(),
        }
    }

    /// Build a 402 Payment Required response based on protocol version
    pub fn build_402_body(&self, resource: &str) -> serde_json::Value {
        match &self.currency {
            PaymentCurrency::Usdc { network, asset } => {
                // x402 v2 — base64-encoded header
                serde_json::json!({
                    "x402Version": 2,
                    "accepts": [{
                        "x402Version": 2,
                        "scheme": "exact",
                        "network": network,
                        "maxAmountRequired": self.cost,
                        "resource": resource,
                        "description": self.description,
                        "payToAddress": asset.to_hex(),
                        "asset": asset.to_hex(),
                        "maxTimeoutSeconds": 60,
                    }]
                })
            }
            PaymentCurrency::Starkbot {
                network,
                asset,
                symbol,
                decimals,
                name,
                version,
                facilitator_signer,
            } => {
                // x402 v1 — JSON body with token metadata
                // Cost is human-readable, convert to raw units (cost * 10^decimals)
                let raw_amount = human_to_raw(&self.cost, *decimals);
                serde_json::json!({
                    "x402Version": 1,
                    "accepts": [{
                        "scheme": "permit",
                        "network": network,
                        "maxAmountRequired": raw_amount,
                        "resource": resource,
                        "description": self.description,
                        "mimeType": "application/json",
                        "payTo": self.pay_to.to_hex(),
                        "maxTimeoutSeconds": 300,
                        "asset": asset,
                        "extra": {
                            "token": symbol,
                            "address": asset,
                            "decimals": decimals,
                            "name": name,
                            "version": version,
                            "facilitatorSigner": facilitator_signer,
                            "minimum_amount": true,
                        }
                    }],
                    "error": null,
                })
            }
        }
    }

    /// Build the payment-required header (base64) for USDC (v2) or return None for Starkbot (v1)
    pub fn build_402_header(&self, resource: &str) -> Option<String> {
        match &self.currency {
            PaymentCurrency::Usdc { network, asset } => {
                let payment_required = serde_json::json!({
                    "x402Version": 2,
                    "accepts": [{
                        "x402Version": 2,
                        "scheme": "exact",
                        "network": network,
                        "maxAmountRequired": DomainUint256::from_str(&self.cost)
                            .map(|v| v.to_string())
                            .unwrap_or_else(|_| self.cost.clone()),
                        "resource": resource,
                        "description": self.description,
                        "payToAddress": self.pay_to.to_hex(),
                        "asset": asset.to_hex(),
                        "maxTimeoutSeconds": 60,
                    }]
                });
                let json = serde_json::to_string(&payment_required).ok()?;
                Some(base64::Engine::encode(
                    &base64::engine::general_purpose::STANDARD,
                    json.as_bytes(),
                ))
            }
            PaymentCurrency::Starkbot { .. } => None,
        }
    }

    /// Build the verify request JSON for the facilitator.
    /// Detects x402Version from the actual payment payload (not hardcoded from endpoint config)
    /// so that x402-rs routes to the correct handler for the payload format.
    pub fn build_verify_request(&self, raw_payload: &serde_json::Value, resource: &str) -> serde_json::Value {
        let detected_version = detect_payload_version(raw_payload);

        match &self.currency {
            PaymentCurrency::Usdc { network, asset } => {
                serde_json::json!({
                    "x402Version": detected_version,
                    "paymentPayload": raw_payload,
                    "paymentRequirements": {
                        "scheme": "exact",
                        "network": network,
                        "amount": self.cost,
                        "payTo": self.pay_to.to_hex(),
                        "asset": asset.to_hex(),
                        "maxTimeoutSeconds": 60,
                    }
                })
            }
            PaymentCurrency::Starkbot {
                network,
                asset,
                symbol,
                decimals,
                name,
                version,
                facilitator_signer,
            } => {
                let raw_amount = human_to_raw(&self.cost, *decimals);
                serde_json::json!({
                    "x402Version": detected_version,
                    "paymentPayload": raw_payload,
                    "paymentRequirements": {
                        "scheme": "permit",
                        "network": network,
                        "maxAmountRequired": raw_amount,
                        "resource": resource,
                        "description": self.description,
                        "mimeType": "application/json",
                        "payTo": self.pay_to.to_hex(),
                        "maxTimeoutSeconds": 300,
                        "asset": asset,
                        "extra": {
                            "token": symbol,
                            "address": asset,
                            "decimals": decimals,
                            "name": name,
                            "version": version,
                            "facilitatorSigner": facilitator_signer,
                            "minimum_amount": true,
                        }
                    }
                })
            }
        }
    }

    /// Build the settle request JSON (same structure as verify for both protocols)
    pub fn build_settle_request(&self, raw_payload: &serde_json::Value, resource: &str) -> serde_json::Value {
        self.build_verify_request(raw_payload, resource)
    }

    /// Whether this is a v2 (USDC) endpoint where we can reliably extract payer/nonce
    pub fn is_v2(&self) -> bool {
        matches!(self.currency, PaymentCurrency::Usdc { .. })
    }

    /// Try to extract payer address from the raw payment payload.
    /// Tries both V2 and V1 extraction paths regardless of endpoint config,
    /// so cross-version payloads are handled correctly.
    pub fn extract_payer(&self, raw_payload: &serde_json::Value) -> Option<String> {
        // V2 path: payload.authorization.from
        raw_payload
            .get("payload")
            .and_then(|p| p.get("authorization"))
            .and_then(|a| a.get("from"))
            .and_then(|f| f.as_str())
            .map(|s| s.to_string())
            // V1 fallback: owner/holder/from at top level
            .or_else(|| {
                raw_payload.get("owner")
                    .or_else(|| raw_payload.get("holder"))
                    .or_else(|| raw_payload.get("from"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            })
    }

    /// Try to extract nonce from the raw payment payload.
    /// Tries both V2 and V1 extraction paths regardless of endpoint config.
    pub fn extract_nonce(&self, raw_payload: &serde_json::Value) -> Option<String> {
        // V2 path: payload.authorization.nonce
        raw_payload
            .get("payload")
            .and_then(|p| p.get("authorization"))
            .and_then(|a| a.get("nonce"))
            .and_then(|n| n.as_str())
            .map(|s| s.to_string())
            // V1 fallback: nonce at top level
            .or_else(|| {
                raw_payload.get("nonce")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            })
    }
}

/// Detect x402 version from the actual payment payload structure.
/// This ensures x402-rs routes to the correct handler regardless of endpoint config.
pub fn detect_payload_version(raw_payload: &serde_json::Value) -> u32 {
    // 1. Explicit x402Version field in the payload
    if let Some(v) = raw_payload.get("x402Version").and_then(|v| v.as_u64()) {
        return v as u32;
    }

    // 2. Structural detection: V2 payloads have payload.authorization
    if raw_payload.get("payload").and_then(|p| p.get("authorization")).is_some() {
        return 2;
    }

    // 3. V1 payloads typically have signature at top level
    if raw_payload.get("signature").is_some() {
        return 1;
    }

    // Default to 2 (most common)
    2
}

/// Convert a human-readable amount to raw token units.
/// e.g. "1000" with 18 decimals -> "1000000000000000000000"
fn human_to_raw(amount: &str, decimals: u8) -> String {
    let cleaned = amount.trim();
    let decimals = decimals as usize;

    let (integer_part, frac_part) = if let Some(dot_pos) = cleaned.find('.') {
        let int_str = &cleaned[..dot_pos];
        let frac_str = cleaned[dot_pos + 1..].trim_end_matches('0');
        if frac_str.len() > decimals {
            return "0".to_string(); // overflow
        }
        (int_str, frac_str.to_string())
    } else {
        (cleaned, String::new())
    };

    let padding = decimals - frac_part.len();
    let raw_str = format!("{}{}{}", integer_part, frac_part, "0".repeat(padding));

    let raw_str = raw_str.trim_start_matches('0');
    if raw_str.is_empty() { "0".to_string() } else { raw_str.to_string() }
}

impl DomainUint256 {
    /// Parse from string, returning Result
    pub fn from_str_checked(s: &str) -> Result<Self, String> {
        Self::from_str(s)
    }
}
