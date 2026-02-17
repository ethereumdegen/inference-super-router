use std::env;

use crate::models::DomainEthAddress;

/// Global configuration from environment variables.
/// Per-endpoint config (API keys, models) comes from endpoints.ron.
#[derive(Clone, Debug)]
pub struct GlobalConfig {
    pub port: u16,
    pub bot_wallet_address: DomainEthAddress,
    pub facilitator_url: String,
    pub base_url: Option<String>,
    pub test_mode: bool,
    pub endpoints_config_path: String,

    // USDC (x402 v2) settings
    pub usdc_network: String,
    pub usdc_token_address: DomainEthAddress,

    // Starkbot (x402 v1) settings
    pub starkbot_network: String,
    pub starkbot_token_address: String,
    pub starkbot_token_symbol: String,
    pub starkbot_token_decimals: u8,
    pub starkbot_token_name: String,
    pub starkbot_token_version: String,
    pub facilitator_signer: String,
}

impl GlobalConfig {
    pub fn from_env() -> Result<Self, String> {
        dotenvy::dotenv().ok();

        let test_mode = env::var("TEST_MODE")
            .map(|v| v == "true" || v == "1")
            .unwrap_or(false);

        let port = env::var("PORT")
            .unwrap_or_else(|_| "8080".to_string())
            .parse::<u16>()
            .map_err(|_| "PORT must be a valid port number")?;

        let bot_wallet_address = env::var("BOT_WALLET_ADDRESS")
            .map_err(|_| "BOT_WALLET_ADDRESS is required")?;
        let bot_wallet_address = DomainEthAddress::from_hex(&bot_wallet_address)
            .map_err(|e| format!("Invalid BOT_WALLET_ADDRESS: {}", e))?;

        let facilitator_url = env::var("FACILITATOR_URL")
            .map_err(|_| "FACILITATOR_URL is required")?;

        let base_url = env::var("BASE_URL").ok();

        let endpoints_config_path = env::var("ENDPOINTS_CONFIG")
            .unwrap_or_else(|_| "endpoints.ron".to_string());

        // USDC defaults (Base mainnet)
        let usdc_network = env::var("USDC_NETWORK")
            .unwrap_or_else(|_| "eip155:8453".to_string());
        let usdc_token_address_str = env::var("USDC_TOKEN_ADDRESS")
            .unwrap_or_else(|_| "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913".to_string());
        let usdc_token_address = DomainEthAddress::from_hex(&usdc_token_address_str)
            .map_err(|e| format!("Invalid USDC_TOKEN_ADDRESS: {}", e))?;

        // Starkbot defaults
        let starkbot_network = env::var("STARKBOT_NETWORK")
            .unwrap_or_else(|_| "base".to_string());
        let starkbot_token_address = env::var("STARKBOT_TOKEN_ADDRESS")
            .unwrap_or_else(|_| "0x587Cd533F418825521f3A1daa7CCd1E7339A1B07".to_string());
        let starkbot_token_symbol = env::var("STARKBOT_TOKEN_SYMBOL")
            .unwrap_or_else(|_| "STARKBOT".to_string());
        let starkbot_token_decimals = env::var("STARKBOT_TOKEN_DECIMALS")
            .unwrap_or_else(|_| "18".to_string())
            .parse::<u8>()
            .map_err(|_| "STARKBOT_TOKEN_DECIMALS must be a valid number")?;
        let starkbot_token_name = env::var("STARKBOT_TOKEN_NAME")
            .unwrap_or_else(|_| "StarkBot".to_string());
        let starkbot_token_version = env::var("STARKBOT_TOKEN_VERSION")
            .unwrap_or_else(|_| "1".to_string());
        let facilitator_signer = env::var("FACILITATOR_SIGNER")
            .unwrap_or_default();

        Ok(GlobalConfig {
            port,
            bot_wallet_address,
            facilitator_url,
            base_url,
            test_mode,
            endpoints_config_path,
            usdc_network,
            usdc_token_address,
            starkbot_network,
            starkbot_token_address,
            starkbot_token_symbol,
            starkbot_token_decimals,
            starkbot_token_name,
            starkbot_token_version,
            facilitator_signer,
        })
    }
}
