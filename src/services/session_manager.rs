use moka::sync::Cache;
use std::time::Duration;
use uuid::Uuid;

const DEFAULT_SESSION_TTL_SECS: u64 = 3600; // 1 hour

/// Information stored for each active session.
#[derive(Clone, Debug)]
pub struct SessionInfo {
    pub wallet_address: String,
    pub chain_id: u64,
    pub created_at: i64,
    pub expires_at: i64,
}

/// Manages bearer-token sessions backed by a TTL cache.
///
/// A client establishes a session with a single ERC-8128 handshake,
/// then uses the returned opaque token for subsequent requests.
pub struct SessionManager {
    cache: Cache<String, SessionInfo>,
    ttl_secs: u64,
}

impl SessionManager {
    pub fn new(ttl_secs: u64) -> Self {
        Self {
            cache: Cache::builder()
                .time_to_live(Duration::from_secs(ttl_secs))
                .max_capacity(10_000)
                .build(),
            ttl_secs,
        }
    }

    pub fn from_env() -> Self {
        let ttl = std::env::var("SESSION_TTL_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_SESSION_TTL_SECS);
        Self::new(ttl)
    }

    /// Create a new session for the given wallet, returning (token, expires_at).
    pub fn create_session(&self, wallet_address: &str, chain_id: u64) -> (String, i64) {
        let token = Uuid::new_v4().to_string();
        let now = chrono::Utc::now().timestamp();
        let expires_at = now + self.ttl_secs as i64;

        let info = SessionInfo {
            wallet_address: wallet_address.to_string(),
            chain_id,
            created_at: now,
            expires_at,
        };

        self.cache.insert(token.clone(), info);
        (token, expires_at)
    }

    /// Validate a session token and return the associated session info.
    pub fn validate(&self, token: &str) -> Option<SessionInfo> {
        self.cache.get(token)
    }

    /// TTL in seconds (for informational purposes).
    pub fn ttl_secs(&self) -> u64 {
        self.ttl_secs
    }
}
