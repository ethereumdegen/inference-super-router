/// Identity recovered from a verified ERC-8128 signature.
#[derive(Debug, Clone)]
pub struct Erc8128Identity {
    pub wallet_address: String,
    pub chain_id: u64,
}

/// Headers produced by signing an outgoing request.
#[derive(Debug, Clone)]
pub struct Erc8128SignedHeaders {
    pub signature_input: String,
    pub signature: String,
    pub content_digest: Option<String>,
}

/// Errors that can occur during ERC-8128 verification or signing.
#[derive(Debug)]
pub enum Erc8128Error {
    MissingHeader(&'static str),
    InvalidSignatureInput(String),
    InvalidSignature(String),
    ContentDigestMismatch,
    Expired,
    NotYetValid,
    RecoveryFailed(String),
    AddressMismatch { expected: String, recovered: String },
    SigningFailed(String),
}

impl std::fmt::Display for Erc8128Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingHeader(h) => write!(f, "Missing header: {}", h),
            Self::InvalidSignatureInput(e) => write!(f, "Invalid Signature-Input: {}", e),
            Self::InvalidSignature(e) => write!(f, "Invalid Signature: {}", e),
            Self::ContentDigestMismatch => write!(f, "Content-Digest does not match body"),
            Self::Expired => write!(f, "Signature expired"),
            Self::NotYetValid => write!(f, "Signature not yet valid"),
            Self::RecoveryFailed(e) => write!(f, "EC recovery failed: {}", e),
            Self::AddressMismatch { expected, recovered } => {
                write!(
                    f,
                    "Address mismatch: expected {}, recovered {}",
                    expected, recovered
                )
            }
            Self::SigningFailed(e) => write!(f, "Signing failed: {}", e),
        }
    }
}

impl std::error::Error for Erc8128Error {}
