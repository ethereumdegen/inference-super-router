//! Settlement queue for decoupling x402 settlement from HTTP request flow.
//! Adapted for protocol-agnostic serde_json::Value payloads.

use crate::services::settlement_store::{SettlementStatus, SettlementStore};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::Notify;
use tracing::{debug, info, warn};

pub const DEFAULT_MAX_QUEUE_SIZE: usize = 10_000;

/// A pending settlement waiting to be processed
#[derive(Debug, Clone)]
pub struct PendingSettlement {
    /// Pre-built settle request JSON (protocol-agnostic)
    pub settle_request: serde_json::Value,
    /// Nonce for tracking/deduplication
    pub nonce: String,
}

impl PendingSettlement {
    pub fn new(settle_request: serde_json::Value, nonce: String) -> Self {
        Self { settle_request, nonce }
    }
}

/// FIFO queue for pending settlements backed by SQLite
pub struct SettlementQueue {
    store: Arc<SettlementStore>,
    notify: Arc<Notify>,
    max_size: usize,
    len: AtomicUsize,
}

impl SettlementQueue {
    pub fn new() -> Self {
        Self::with_max_size(DEFAULT_MAX_QUEUE_SIZE)
    }

    pub fn with_max_size(max_size: usize) -> Self {
        Self::with_store_and_max_size(Self::default_db_path(), max_size)
            .expect("Failed to initialize settlement store")
    }

    fn default_db_path() -> &'static str {
        "data/settlements.db"
    }

    pub fn with_store_and_max_size(
        db_path: &str,
        max_size: usize,
    ) -> Result<Self, crate::services::settlement_store::SettlementStoreError> {
        if let Some(parent) = std::path::Path::new(db_path).parent() {
            std::fs::create_dir_all(parent).ok();
        }

        let store = Arc::new(SettlementStore::open(db_path)?);

        let recovered = store.recover_in_progress()?;
        if recovered > 0 {
            info!("Recovered {} in-progress settlements from previous session", recovered);
        }

        let pending = store.pending_count()? as usize;
        let in_progress = store.in_progress_count()? as usize;
        let initial_len = pending + in_progress;

        if initial_len > 0 {
            info!(
                "Loaded {} pending settlements from database ({} pending, {} in-progress)",
                initial_len, pending, in_progress
            );
        }

        Ok(Self {
            store,
            notify: Arc::new(Notify::new()),
            max_size,
            len: AtomicUsize::new(initial_len),
        })
    }

    pub fn store(&self) -> &Arc<SettlementStore> {
        &self.store
    }

    pub async fn push(&self, settlement: PendingSettlement) -> Result<(), PendingSettlement> {
        let current_len = self.len.load(Ordering::SeqCst);

        if current_len >= self.max_size {
            warn!(
                "Settlement queue full ({}/{}), rejecting settlement for nonce {}",
                current_len, self.max_size, settlement.nonce
            );
            return Err(settlement);
        }

        match self.store.insert(&settlement.nonce, &settlement.settle_request) {
            Ok(Some(_id)) => {
                let new_len = self.len.fetch_add(1, Ordering::SeqCst) + 1;
                debug!("Queuing settlement for nonce {} (persisted), queue depth: {}", settlement.nonce, new_len);
                self.notify.notify_one();
                Ok(())
            }
            Ok(None) => {
                debug!("Settlement for nonce {} already exists, skipping", settlement.nonce);
                Ok(())
            }
            Err(e) => {
                warn!("Failed to persist settlement for nonce {}: {}", settlement.nonce, e);
                Err(settlement)
            }
        }
    }

    pub fn len(&self) -> usize {
        self.len.load(Ordering::SeqCst)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn is_full(&self) -> bool {
        self.len() >= self.max_size
    }

    pub fn max_size(&self) -> usize {
        self.max_size
    }

    pub fn notify_all(&self) {
        self.notify.notify_waiters();
    }

    pub async fn wait_for_items(&self) {
        self.notify.notified().await;
    }

    pub fn get_status_counts(&self) -> (i64, i64, i64, i64) {
        let pending = self.store.pending_count().unwrap_or(0);
        let in_progress = self.store.in_progress_count().unwrap_or(0);
        let completed = self.store.count_by_status(SettlementStatus::Completed).unwrap_or(0);
        let failed = self.store.count_by_status(SettlementStatus::Failed).unwrap_or(0);
        (pending, in_progress, completed, failed)
    }
}

impl Default for SettlementQueue {
    fn default() -> Self {
        Self::new()
    }
}
