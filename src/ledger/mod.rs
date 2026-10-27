use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

#[derive(Error, Debug, PartialEq, Eq, Clone)]
pub enum LedgerError {
    #[error("Ledger sequence number mismatch at index {index}: expected {expected}, found {found}")]
    SequenceMismatch {
        index: usize,
        expected: u64,
        found: u64,
    },
    #[error("Ledger previous hash link broken at seq {seq_no}: expected {expected}, found {found}")]
    BrokenPrevHashLink {
        seq_no: u64,
        expected: String,
        found: String,
    },
    #[error("Ledger entry hash tampered at seq {seq_no}: computed {computed}, declared {declared}")]
    EntryHashTampered {
        seq_no: u64,
        computed: String,
        declared: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event_type", content = "payload")]
pub enum LedgerEvent {
    MarketRegistered {
        market_id: String,
        origin_key: String,
        question: String,
    },
    TentativeOutcomeObserved {
        market_id: String,
        outcome_code: u8,
        first_seen_at: i64,
        payload_commitment: [u8; 32],
    },
    TentativeOutcomeReverted {
        market_id: String,
        reverted_outcome_code: u8,
        reverted_at: i64,
        payload_commitment: [u8; 32],
        reason: String,
    },
    OutcomeProposed {
        market_id: String,
        outcome_code: u8,
        proposed_at: i64,
        expires_at: i64,
        bond_amount: Decimal,
        payload_commitment: [u8; 32],
    },
    DisputeChallenged {
        market_id: String,
        round: u32,
        counter_outcome: u8,
        bond_amount: Decimal,
        challenger: String,
    },
    MarketFinalized {
        market_id: String,
        final_outcome: u8,
        finalized_at: i64,
        receipt_hash: [u8; 32],
    },
    MarketVoided {
        market_id: String,
        void_code: u8,
        reason: String,
        voided_at: i64,
        receipt_hash: [u8; 32],
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerEntry {
    pub seq_no: u64,
    pub timestamp: i64,
    pub prev_hash: [u8; 32],
    pub entry_hash: [u8; 32],
    pub event: LedgerEvent,
}

impl LedgerEntry {
    pub fn compute_hash(
        seq_no: u64,
        timestamp: i64,
        prev_hash: &[u8; 32],
        event: &LedgerEvent,
    ) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(b"RAAS_LEDGER_V1:");
        hasher.update(&seq_no.to_be_bytes());
        hasher.update(&timestamp.to_be_bytes());
        hasher.update(prev_hash);
        let serialized_event = serde_json::to_vec(event).unwrap_or_default();
        hasher.update(&serialized_event);

        let digest = hasher.finalize();
        let mut out = [0u8; 32];
        out.copy_from_slice(&digest);
        out
    }
}

/// Append-only, cryptographically hash-chained audit log for market resolution state transitions.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditLedger {
    pub entries: Vec<LedgerEntry>,
}

impl AuditLedger {
    pub const GENESIS_HASH: [u8; 32] = [0u8; 32];

    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    pub fn head_hash(&self) -> [u8; 32] {
        self.entries
            .last()
            .map(|e| e.entry_hash)
            .unwrap_or(Self::GENESIS_HASH)
    }

    pub fn append(&mut self, timestamp: i64, event: LedgerEvent) -> LedgerEntry {
        let seq_no = self.entries.len() as u64;
        let prev_hash = self.head_hash();
        let entry_hash = LedgerEntry::compute_hash(seq_no, timestamp, &prev_hash, &event);

        let entry = LedgerEntry {
            seq_no,
            timestamp,
            prev_hash,
            entry_hash,
            event,
        };
        self.entries.push(entry.clone());
        entry
    }

    /// Recomputes and verifies every hash link from GENESIS_HASH (`[0u8; 32]`) to the current head.
    pub fn verify_chain_integrity(&self) -> Result<(), LedgerError> {
        let mut expected_prev = Self::GENESIS_HASH;

        for (idx, entry) in self.entries.iter().enumerate() {
            let expected_seq = idx as u64;
            if entry.seq_no != expected_seq {
                return Err(LedgerError::SequenceMismatch {
                    index: idx,
                    expected: expected_seq,
                    found: entry.seq_no,
                });
            }

            if entry.prev_hash != expected_prev {
                return Err(LedgerError::BrokenPrevHashLink {
                    seq_no: entry.seq_no,
                    expected: hex::encode(expected_prev),
                    found: hex::encode(entry.prev_hash),
                });
            }

            let computed = LedgerEntry::compute_hash(
                entry.seq_no,
                entry.timestamp,
                &entry.prev_hash,
                &entry.event,
            );
            if computed != entry.entry_hash {
                return Err(LedgerError::EntryHashTampered {
                    seq_no: entry.seq_no,
                    computed: hex::encode(computed),
                    declared: hex::encode(entry.entry_hash),
                });
            }

            expected_prev = entry.entry_hash;
        }

        Ok(())
    }
}
