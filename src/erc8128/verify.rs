//! ERC-8128 HTTP request signature verification (RFC 9421 + ERC-191)
//!
//! Verifies signatures produced by ERC-8128 signers, recovering the Ethereum
//! address via secp256k1 ecrecover and comparing against the `keyid`.

use super::types::{Erc8128Error, Erc8128Identity};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use sha2::{Digest as Sha2Digest, Sha256};
use sha3::Keccak256;

/// Check if the request has ERC-8128 signature headers.
pub fn has_erc8128_headers(headers: &actix_web::http::header::HeaderMap) -> bool {
    headers.contains_key("signature-input") && headers.contains_key("signature")
}

/// Parsed fields from the Signature-Input header.
struct SigInputParsed {
    components: Vec<String>,
    created: i64,
    expires: i64,
    keyid: String,
    #[allow(dead_code)]
    nonce: String,
    /// The raw `@signature-params` line value (everything after `eth=`)
    sig_params: String,
}

/// Parse `Signature-Input: eth=("@method" "@authority" ...);created=T;expires=T;keyid="...";nonce="...";alg="erc191"`
fn parse_signature_input(value: &str) -> Result<SigInputParsed, Erc8128Error> {
    let rest = value
        .strip_prefix("eth=")
        .ok_or_else(|| Erc8128Error::InvalidSignatureInput("must start with 'eth='".into()))?;

    let sig_params = rest.to_string();

    let paren_open = rest
        .find('(')
        .ok_or_else(|| Erc8128Error::InvalidSignatureInput("missing '('".into()))?;
    let paren_close = rest
        .find(')')
        .ok_or_else(|| Erc8128Error::InvalidSignatureInput("missing ')'".into()))?;

    let components_str = &rest[paren_open + 1..paren_close];
    let components: Vec<String> = components_str
        .split_whitespace()
        .map(|s| s.trim_matches('"').to_string())
        .collect();

    let params_str = &rest[paren_close + 1..];
    let mut created: Option<i64> = None;
    let mut expires: Option<i64> = None;
    let mut keyid: Option<String> = None;
    let mut nonce: Option<String> = None;

    for part in params_str.split(';') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some((k, v)) = part.split_once('=') {
            let v = v.trim_matches('"');
            match k {
                "created" => created = v.parse().ok(),
                "expires" => expires = v.parse().ok(),
                "keyid" => keyid = Some(v.to_string()),
                "nonce" => nonce = Some(v.to_string()),
                "alg" => {}
                _ => {}
            }
        }
    }

    Ok(SigInputParsed {
        components,
        created: created
            .ok_or_else(|| Erc8128Error::InvalidSignatureInput("missing 'created'".into()))?,
        expires: expires
            .ok_or_else(|| Erc8128Error::InvalidSignatureInput("missing 'expires'".into()))?,
        keyid: keyid
            .ok_or_else(|| Erc8128Error::InvalidSignatureInput("missing 'keyid'".into()))?,
        nonce: nonce
            .ok_or_else(|| Erc8128Error::InvalidSignatureInput("missing 'nonce'".into()))?,
        sig_params,
    })
}

/// Parse `Signature: eth=:<base64 65-byte sig>:`
fn parse_signature(value: &str) -> Result<[u8; 65], Erc8128Error> {
    let rest = value
        .strip_prefix("eth=:")
        .ok_or_else(|| Erc8128Error::InvalidSignature("must start with 'eth=:'".into()))?;
    let b64 = rest
        .strip_suffix(':')
        .ok_or_else(|| Erc8128Error::InvalidSignature("must end with ':'".into()))?;

    let bytes = BASE64
        .decode(b64)
        .map_err(|e| Erc8128Error::InvalidSignature(format!("base64 decode: {}", e)))?;

    if bytes.len() != 65 {
        return Err(Erc8128Error::InvalidSignature(format!(
            "expected 65 bytes, got {}",
            bytes.len()
        )));
    }

    let mut sig = [0u8; 65];
    sig.copy_from_slice(&bytes);
    Ok(sig)
}

/// Compute SHA-256 Content-Digest in RFC 9530 format: `sha-256=:<base64>:`
fn content_digest_sha256(body: &[u8]) -> String {
    let hash = Sha256::digest(body);
    let encoded = BASE64.encode(hash);
    format!("sha-256=:{}:", encoded)
}

/// EIP-191 hash: keccak256("\x19Ethereum Signed Message:\n{len}{message}")
fn eip191_hash(message: &[u8]) -> [u8; 32] {
    let prefix = format!("\x19Ethereum Signed Message:\n{}", message.len());
    let mut hasher = Keccak256::new();
    hasher.update(prefix.as_bytes());
    hasher.update(message);
    hasher.finalize().into()
}

/// Recover Ethereum address from a 65-byte signature and message hash.
fn ecrecover(hash: &[u8; 32], sig: &[u8; 65]) -> Result<String, Erc8128Error> {
    use k256::ecdsa::{RecoveryId, Signature, VerifyingKey};

    let signature = Signature::from_slice(&sig[..64])
        .map_err(|e| Erc8128Error::RecoveryFailed(format!("invalid signature: {}", e)))?;

    let v = if sig[64] >= 27 { sig[64] - 27 } else { sig[64] };
    let recovery_id = RecoveryId::new(v != 0, false);

    let verifying_key = VerifyingKey::recover_from_prehash(hash, &signature, recovery_id)
        .map_err(|e| Erc8128Error::RecoveryFailed(format!("recovery: {}", e)))?;

    let pubkey_bytes = verifying_key.to_encoded_point(false);
    let pubkey_uncompressed = pubkey_bytes.as_bytes();

    let mut hasher = Keccak256::new();
    hasher.update(&pubkey_uncompressed[1..]);
    let hash = hasher.finalize();

    let addr = format!("0x{}", hex::encode(&hash[12..]));
    Ok(addr)
}

/// Verify an ERC-8128 signed HTTP request from an actix-web `HttpRequest`.
///
/// Extracts method, authority, path, query, and signature headers from the request.
/// `body` must be passed separately (actix-web consumes the body before the handler).
pub fn verify_from_request(
    req: &actix_web::HttpRequest,
    body: &[u8],
) -> Result<Erc8128Identity, Erc8128Error> {
    let method = req.method().as_str();
    let authority = req
        .headers()
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let path = req.path();
    let query = req.query_string();
    let query = if query.is_empty() { None } else { Some(query) };

    verify_erc8128(method, authority, path, query, body, req.headers())
}

/// Verify an ERC-8128 signed HTTP request.
fn verify_erc8128(
    method: &str,
    authority: &str,
    path: &str,
    query: Option<&str>,
    body: &[u8],
    headers: &actix_web::http::header::HeaderMap,
) -> Result<Erc8128Identity, Erc8128Error> {
    // 1. Extract headers
    let sig_input_value = headers
        .get("signature-input")
        .and_then(|v| v.to_str().ok())
        .ok_or(Erc8128Error::MissingHeader("Signature-Input"))?;

    let sig_value = headers
        .get("signature")
        .and_then(|v| v.to_str().ok())
        .ok_or(Erc8128Error::MissingHeader("Signature"))?;

    // 2. Parse Signature-Input and Signature
    let input = parse_signature_input(sig_input_value)?;
    let sig_bytes = parse_signature(sig_value)?;

    // 3. Verify Content-Digest if body is present
    if !body.is_empty() {
        let expected_digest = content_digest_sha256(body);
        let actual_digest = headers
            .get("content-digest")
            .and_then(|v| v.to_str().ok())
            .ok_or(Erc8128Error::MissingHeader("Content-Digest"))?;

        if expected_digest != actual_digest {
            return Err(Erc8128Error::ContentDigestMismatch);
        }
    }

    // 4. Validate timestamps (created <= now+60, expires > now)
    let now = chrono::Utc::now().timestamp();
    if input.created > now + 60 {
        return Err(Erc8128Error::NotYetValid);
    }
    if input.expires <= now {
        return Err(Erc8128Error::Expired);
    }

    // 5. Rebuild RFC 9421 signature base
    let mut base_lines: Vec<String> = Vec::new();
    for comp in &input.components {
        let value = match comp.as_str() {
            "@method" => method.to_uppercase(),
            "@authority" => authority.to_lowercase(),
            "@path" => path.to_string(),
            "@query" => format!("?{}", query.unwrap_or("")),
            "content-digest" => headers
                .get("content-digest")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string(),
            _ => String::new(),
        };
        base_lines.push(format!("\"{}\": {}", comp, value));
    }

    base_lines.push(format!("\"@signature-params\": {}", input.sig_params));
    let signature_base = base_lines.join("\n");

    // 6. EIP-191 hash the signature base
    let hash = eip191_hash(signature_base.as_bytes());

    // 7. Recover address
    let recovered = ecrecover(&hash, &sig_bytes)?;

    // 8. Parse keyid: "erc8128:{chain_id}:{address}"
    let keyid_parts: Vec<&str> = input.keyid.splitn(3, ':').collect();
    if keyid_parts.len() != 3 || keyid_parts[0] != "erc8128" {
        return Err(Erc8128Error::InvalidSignatureInput(
            "keyid must be 'erc8128:{chain}:{addr}'".into(),
        ));
    }
    let chain_id: u64 = keyid_parts[1]
        .parse()
        .map_err(|_| Erc8128Error::InvalidSignatureInput("invalid chain_id in keyid".into()))?;
    let expected_address = keyid_parts[2].to_lowercase();

    // 9. Compare addresses (case-insensitive)
    if recovered.to_lowercase() != expected_address {
        return Err(Erc8128Error::AddressMismatch {
            expected: expected_address,
            recovered,
        });
    }

    Ok(Erc8128Identity {
        wallet_address: recovered,
        chain_id,
    })
}
