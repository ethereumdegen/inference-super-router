//! SQLite-backed persistent storage for settlements.
//! Adapted for protocol-agnostic serde_json::Value payloads.

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::sync::Mutex;
use tracing::{debug, error, info, warn};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettlementStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
}

impl SettlementStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            SettlementStatus::Pending => "pending",
            SettlementStatus::InProgress => "in_progress",
            SettlementStatus::Completed => "completed",
            SettlementStatus::Failed => "failed",
        }
    }

    fn from_str(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(SettlementStatus::Pending),
            "in_progress" => Some(SettlementStatus::InProgress),
            "completed" => Some(SettlementStatus::Completed),
            "failed" => Some(SettlementStatus::Failed),
            _ => None,
        }
    }
}

/// A settlement record stored in the database
#[derive(Debug, Clone)]
pub struct StoredSettlement {
    pub id: i64,
    pub nonce: String,
    /// The settle request JSON (ready to POST to /settle)
    pub settle_request_json: String,
    pub queued_at: DateTime<Utc>,
    pub status: SettlementStatus,
    pub retry_count: i32,
    pub last_error: Option<String>,
    pub tx_hash: Option<String>,
    pub updated_at: DateTime<Utc>,
}

impl StoredSettlement {
    /// Deserialize the settle request
    pub fn settle_request(&self) -> Result<serde_json::Value, serde_json::Error> {
        serde_json::from_str(&self.settle_request_json)
    }
}

pub struct SettlementStore {
    conn: Mutex<Connection>,
}

impl SettlementStore {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, rusqlite::Error> {
        let conn = Connection::open(path)?;

        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;")?;

        conn.execute(
            "CREATE TABLE IF NOT EXISTS settlements (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                nonce TEXT UNIQUE NOT NULL,
                settle_request_json TEXT NOT NULL,
                queued_at TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'pending',
                retry_count INTEGER NOT NULL DEFAULT 0,
                last_error TEXT,
                tx_hash TEXT,
                updated_at TEXT NOT NULL
            )",
            [],
        )?;

        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_settlements_status ON settlements(status)",
            [],
        )?;

        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_settlements_nonce ON settlements(nonce)",
            [],
        )?;

        info!("Settlement store initialized");
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self, rusqlite::Error> {
        Self::open(":memory:")
    }

    /// Insert a new pending settlement.
    /// Takes the pre-built settle request JSON (protocol-agnostic).
    pub fn insert(
        &self,
        nonce: &str,
        settle_request: &serde_json::Value,
    ) -> Result<Option<i64>, SettlementStoreError> {
        let request_json = serde_json::to_string(settle_request)?;
        let now = Utc::now().to_rfc3339();

        let conn = self.conn.lock().unwrap();

        let rows_affected = conn.execute(
            "INSERT OR IGNORE INTO settlements
             (nonce, settle_request_json, queued_at, status, updated_at)
             VALUES (?1, ?2, ?3, 'pending', ?3)",
            params![nonce, request_json, now],
        )?;

        if rows_affected == 0 {
            debug!("Settlement for nonce {} already exists in store", nonce);
            return Ok(None);
        }

        let id = conn.last_insert_rowid();
        debug!("Inserted settlement {} for nonce {}", id, nonce);
        Ok(Some(id))
    }

    pub fn claim_next(&self) -> Result<Option<StoredSettlement>, SettlementStoreError> {
        let conn = self.conn.lock().unwrap();
        let now = Utc::now().to_rfc3339();

        let result: Option<i64> = conn
            .query_row(
                "UPDATE settlements
                 SET status = 'in_progress', updated_at = ?1
                 WHERE id = (
                     SELECT id FROM settlements
                     WHERE status = 'pending'
                     ORDER BY queued_at ASC
                     LIMIT 1
                 )
                 RETURNING id",
                params![now],
                |row| row.get(0),
            )
            .optional()?;

        match result {
            Some(id) => self.get_by_id_internal(&conn, id),
            None => Ok(None),
        }
    }

    fn get_by_id_internal(
        &self,
        conn: &Connection,
        id: i64,
    ) -> Result<Option<StoredSettlement>, SettlementStoreError> {
        let result = conn
            .query_row(
                "SELECT id, nonce, settle_request_json,
                        queued_at, status, retry_count, last_error, tx_hash, updated_at
                 FROM settlements WHERE id = ?1",
                params![id],
                |row| {
                    Ok(StoredSettlement {
                        id: row.get(0)?,
                        nonce: row.get(1)?,
                        settle_request_json: row.get(2)?,
                        queued_at: DateTime::parse_from_rfc3339(&row.get::<_, String>(3)?)
                            .map(|dt| dt.with_timezone(&Utc))
                            .unwrap_or_else(|_| Utc::now()),
                        status: SettlementStatus::from_str(&row.get::<_, String>(4)?)
                            .unwrap_or(SettlementStatus::Pending),
                        retry_count: row.get(5)?,
                        last_error: row.get(6)?,
                        tx_hash: row.get(7)?,
                        updated_at: DateTime::parse_from_rfc3339(&row.get::<_, String>(8)?)
                            .map(|dt| dt.with_timezone(&Utc))
                            .unwrap_or_else(|_| Utc::now()),
                    })
                },
            )
            .optional()?;

        Ok(result)
    }

    pub fn mark_completed(&self, id: i64, tx_hash: &str) -> Result<(), SettlementStoreError> {
        let conn = self.conn.lock().unwrap();
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "UPDATE settlements SET status = 'completed', tx_hash = ?1, updated_at = ?2 WHERE id = ?3",
            params![tx_hash, now, id],
        )?;
        debug!("Marked settlement {} as completed, tx: {}", id, tx_hash);
        Ok(())
    }

    pub fn mark_failed(&self, id: i64, error: &str) -> Result<(), SettlementStoreError> {
        let conn = self.conn.lock().unwrap();
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "UPDATE settlements SET status = 'failed', last_error = ?1, updated_at = ?2 WHERE id = ?3",
            params![error, now, id],
        )?;
        warn!("Marked settlement {} as failed: {}", id, error);
        Ok(())
    }

    pub fn record_retry(&self, id: i64, error: &str) -> Result<(), SettlementStoreError> {
        let conn = self.conn.lock().unwrap();
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "UPDATE settlements SET status = 'pending', retry_count = retry_count + 1,
             last_error = ?1, updated_at = ?2 WHERE id = ?3",
            params![error, now, id],
        )?;
        debug!("Recorded retry for settlement {}: {}", id, error);
        Ok(())
    }

    pub fn count_by_status(&self, status: SettlementStatus) -> Result<i64, SettlementStoreError> {
        let conn = self.conn.lock().unwrap();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM settlements WHERE status = ?1",
            params![status.as_str()],
            |row| row.get(0),
        )?;
        Ok(count)
    }

    pub fn pending_count(&self) -> Result<i64, SettlementStoreError> {
        self.count_by_status(SettlementStatus::Pending)
    }

    pub fn in_progress_count(&self) -> Result<i64, SettlementStoreError> {
        self.count_by_status(SettlementStatus::InProgress)
    }

    pub fn recover_in_progress(&self) -> Result<i64, SettlementStoreError> {
        let conn = self.conn.lock().unwrap();
        let now = Utc::now().to_rfc3339();
        let count = conn.execute(
            "UPDATE settlements SET status = 'pending', updated_at = ?1 WHERE status = 'in_progress'",
            params![now],
        )?;
        if count > 0 {
            info!("Recovered {} in-progress settlements back to pending", count);
        }
        Ok(count as i64)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SettlementStoreError {
    #[error("Database error: {0}")]
    Database(#[from] rusqlite::Error),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mock_settle_request() -> serde_json::Value {
        serde_json::json!({
            "x402Version": 2,
            "paymentPayload": {"test": true},
            "paymentRequirements": {"scheme": "exact"}
        })
    }

    #[test]
    fn test_insert_and_claim() {
        let store = SettlementStore::open_in_memory().unwrap();
        let request = mock_settle_request();

        let id = store.insert("nonce1", &request).unwrap().unwrap();
        assert!(id > 0);

        let settlement = store.claim_next().unwrap().unwrap();
        assert_eq!(settlement.nonce, "nonce1");
        assert_eq!(settlement.status, SettlementStatus::InProgress);

        assert!(store.claim_next().unwrap().is_none());
    }

    #[test]
    fn test_duplicate_nonce() {
        let store = SettlementStore::open_in_memory().unwrap();
        let request = mock_settle_request();

        let id1 = store.insert("nonce1", &request).unwrap();
        assert!(id1.is_some());

        let id2 = store.insert("nonce1", &request).unwrap();
        assert!(id2.is_none());
    }

    #[test]
    fn test_mark_completed() {
        let store = SettlementStore::open_in_memory().unwrap();
        let request = mock_settle_request();

        store.insert("nonce1", &request).unwrap();
        let settlement = store.claim_next().unwrap().unwrap();
        store.mark_completed(settlement.id, "0xabc123").unwrap();

        assert_eq!(store.pending_count().unwrap(), 0);
        assert_eq!(store.count_by_status(SettlementStatus::Completed).unwrap(), 1);
    }

    #[test]
    fn test_fifo_order() {
        let store = SettlementStore::open_in_memory().unwrap();
        let request = mock_settle_request();

        store.insert("first", &request).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        store.insert("second", &request).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        store.insert("third", &request).unwrap();

        assert_eq!(store.claim_next().unwrap().unwrap().nonce, "first");
        assert_eq!(store.claim_next().unwrap().unwrap().nonce, "second");
        assert_eq!(store.claim_next().unwrap().unwrap().nonce, "third");
    }

    #[test]
    fn test_recover_in_progress() {
        let store = SettlementStore::open_in_memory().unwrap();
        let request = mock_settle_request();

        store.insert("nonce1", &request).unwrap();
        store.claim_next().unwrap();

        assert_eq!(store.in_progress_count().unwrap(), 1);
        assert_eq!(store.pending_count().unwrap(), 0);

        store.recover_in_progress().unwrap();

        assert_eq!(store.in_progress_count().unwrap(), 0);
        assert_eq!(store.pending_count().unwrap(), 1);
    }
}
