//! ERC-8128 HTTP request signing for outgoing admin requests.
//!
//! Signs requests using a private key with RFC 9421 signature base + EIP-191 hashing.

use super::types::{Erc8128Error, Erc8128SignedHeaders};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use k256::ecdsa::SigningKey;
use sha2::{Digest as Sha2Digest, Sha256};
use sha3::Keccak256;

/// Signs outgoing HTTP requests with an admin private key using ERC-8128 (RFC 9421 + EIP-191).
#[derive(Clone)]
pub struct Erc8128Signer {
    signing_key: SigningKey,
    address: String,
    chain_id: u64,
}

impl Erc8128Signer {
    /// Create a new signer from a hex-encoded private key (with or without 0x prefix).
    pub fn from_private_key(key_hex: &str, chain_id: u64) -> Result<Self, Erc8128Error> {
        let key_hex = key_hex.strip_prefix("0x").unwrap_or(key_hex);
        let key_bytes = hex::decode(key_hex)
            .map_err(|e| Erc8128Error::SigningFailed(format!("invalid hex key: {}", e)))?;

        let signing_key = SigningKey::from_slice(&key_bytes)
            .map_err(|e| Erc8128Error::SigningFailed(format!("invalid private key: {}", e)))?;

        // Derive address from public key
        let verifying_key = signing_key.verifying_key();
        let pubkey_bytes = verifying_key.to_encoded_point(false);
        let pubkey_uncompressed = pubkey_bytes.as_bytes();

        let mut hasher = Keccak256::new();
        hasher.update(&pubkey_uncompressed[1..]);
        let hash = hasher.finalize();
        let address = format!("0x{}", hex::encode(&hash[12..]));

        Ok(Self {
            signing_key,
            address,
            chain_id,
        })
    }

    /// Get the admin wallet address derived from the private key.
    pub fn address(&self) -> &str {
        &self.address
    }

    /// Sign an outgoing HTTP request, returning the headers to attach.
    pub fn sign_request(
        &self,
        method: &str,
        authority: &str,
        path: &str,
        query: Option<&str>,
        body: Option<&[u8]>,
    ) -> Result<Erc8128SignedHeaders, Erc8128Error> {
        let now = chrono::Utc::now().timestamp();
        let expires = now + 300; // 5 minute validity
        let nonce = uuid::Uuid::new_v4().to_string();

        let keyid = format!("erc8128:{}:{}", self.chain_id, self.address);

        // Compute content digest if body present
        let content_digest = body.filter(|b| !b.is_empty()).map(|b| {
            let hash = Sha256::digest(b);
            let encoded = BASE64.encode(hash);
            format!("sha-256=:{}:", encoded)
        });

        // Build component list — include content-digest only if body present
        let mut components = vec![
            "@method".to_string(),
            "@authority".to_string(),
            "@path".to_string(),
            "@query".to_string(),
        ];
        if content_digest.is_some() {
            components.push("content-digest".to_string());
        }

        let components_str = components
            .iter()
            .map(|c| format!("\"{}\"", c))
            .collect::<Vec<_>>()
            .join(" ");

        let sig_params = format!(
            "({});created={};expires={};keyid=\"{}\";nonce=\"{}\";alg=\"erc191\"",
            components_str, now, expires, keyid, nonce
        );

        // Build RFC 9421 signature base
        let mut base_lines: Vec<String> = Vec::new();
        for comp in &components {
            let value = match comp.as_str() {
                "@method" => method.to_uppercase(),
                "@authority" => authority.to_lowercase(),
                "@path" => path.to_string(),
                "@query" => format!("?{}", query.unwrap_or("")),
                "content-digest" => content_digest.clone().unwrap_or_default(),
                _ => String::new(),
            };
            base_lines.push(format!("\"{}\": {}", comp, value));
        }
        base_lines.push(format!("\"@signature-params\": {}", sig_params));
        let signature_base = base_lines.join("\n");

        // EIP-191 hash
        let prefix = format!(
            "\x19Ethereum Signed Message:\n{}",
            signature_base.len()
        );
        let mut hasher = Keccak256::new();
        hasher.update(prefix.as_bytes());
        hasher.update(signature_base.as_bytes());
        let hash: [u8; 32] = hasher.finalize().into();

        // Sign with recoverable signature
        let (signature, recovery_id) = self
            .signing_key
            .sign_prehash_recoverable(&hash)
            .map_err(|e| Erc8128Error::SigningFailed(format!("sign_prehash: {}", e)))?;

        // Build 65-byte signature: r(32) + s(32) + v(1)
        let mut sig_bytes = [0u8; 65];
        sig_bytes[..64].copy_from_slice(&signature.to_bytes());
        sig_bytes[64] = recovery_id.to_byte() + 27; // EIP-155 style v value

        let sig_b64 = BASE64.encode(sig_bytes);

        Ok(Erc8128SignedHeaders {
            signature_input: format!("eth={}", sig_params),
            signature: format!("eth=:{}:", sig_b64),
            content_digest,
        })
    }
}
