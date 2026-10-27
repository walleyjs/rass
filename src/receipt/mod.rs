use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

pub const PAYOUT_DENOMINATOR: u64 = 10_000;
pub const RECEIPT_WIRE_LEN: usize = 153;
pub const DOMAIN_SEPARATOR: &[u8; 24] = b"RAAS_ONCHAIN_RECEIPT_V1:";

#[derive(Error, Debug, PartialEq, Eq, Clone)]
pub enum ReceiptError {
    #[error("Unsupported outcome code {0}: expected 1 (YES), 2 (NO), or 3 (VOID)")]
    UnsupportedOutcomeCode(u8),
    #[error("Market ID hash mismatch: computed {computed}, declared {declared}")]
    MarketIdHashMismatch { computed: String, declared: String },
    #[error("Invalid payout vector {numerators:?} / {denominator} for outcome {outcome}")]
    InvalidPayoutVector {
        outcome: u8,
        numerators: [u64; 2],
        denominator: u64,
    },
    #[error("Invalid oracle public key: {0}")]
    InvalidPublicKey(String),
    #[error("Invalid Ed25519 signature over 153-byte wire receipt: {0}")]
    InvalidSignature(String),
}

/// Compact, binary-deterministic settlement receipt designed for direct verification by
/// Solana (`ed25519_program` precompile) and EVM smart contracts (`reportPayouts`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettlementReceipt {
    pub market_id: String,
    pub market_id_hash: [u8; 32],
    /// 1 = YES, 2 = NO, 3 = VOID
    pub final_outcome: u8,
    /// Basis-point payout numerators: `[10_000, 0]` (YES), `[0, 10_000]` (NO), `[5_000, 5_000]` (VOID)
    pub payout_numerators: [u64; 2],
    pub payout_denominator: u64,
    pub attestation_commitment: [u8; 32],
    pub ledger_head_hash: [u8; 32],
    pub finalized_at: i64,
    pub oracle_pubkey: [u8; 32],
    pub oracle_signature: Vec<u8>,
}

impl SettlementReceipt {
    pub fn hash_market_id(market_id: &str) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(market_id.as_bytes());
        let digest = hasher.finalize();
        let mut out = [0u8; 32];
        out.copy_from_slice(&digest);
        out
    }

    pub fn canonical_payout_numerators(outcome: u8) -> Result<[u64; 2], ReceiptError> {
        match outcome {
            1 => Ok([10_000, 0]),
            2 => Ok([0, 10_000]),
            3 => Ok([5_000, 5_000]),
            other => Err(ReceiptError::UnsupportedOutcomeCode(other)),
        }
    }

    /// Encodes the exact 153-byte deterministic binary wire layout signed by the oracle:
    /// - `[0..24]`   : `"RAAS_ONCHAIN_RECEIPT_V1:"` (24 bytes)
    /// - `[24..56]`  : `market_id_hash` (32 bytes)
    /// - `[56]`      : `final_outcome` (1 byte)
    /// - `[57..65]`  : `payout_numerators[0]` BE (8 bytes)
    /// - `[65..73]`  : `payout_numerators[1]` BE (8 bytes)
    /// - `[73..81]`  : `payout_denominator` BE (8 bytes)
    /// - `[81..113]` : `attestation_commitment` (32 bytes)
    /// - `[113..145]`: `ledger_head_hash` (32 bytes)
    /// - `[145..153]`: `finalized_at` BE (8 bytes)
    pub fn to_wire_bytes(&self) -> [u8; RECEIPT_WIRE_LEN] {
        let mut buf = [0u8; RECEIPT_WIRE_LEN];
        buf[0..24].copy_from_slice(DOMAIN_SEPARATOR);
        buf[24..56].copy_from_slice(&self.market_id_hash);
        buf[56] = self.final_outcome;
        buf[57..65].copy_from_slice(&self.payout_numerators[0].to_be_bytes());
        buf[65..73].copy_from_slice(&self.payout_numerators[1].to_be_bytes());
        buf[73..81].copy_from_slice(&self.payout_denominator.to_be_bytes());
        buf[81..113].copy_from_slice(&self.attestation_commitment);
        buf[113..145].copy_from_slice(&self.ledger_head_hash);
        buf[145..153].copy_from_slice(&self.finalized_at.to_be_bytes());
        buf
    }

    pub fn receipt_hash(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(&self.to_wire_bytes());
        hasher.update(&self.oracle_signature);
        let digest = hasher.finalize();
        let mut out = [0u8; 32];
        out.copy_from_slice(&digest);
        out
    }

    pub fn issue(
        signing_key: &SigningKey,
        market_id: impl Into<String>,
        final_outcome: u8,
        attestation_commitment: [u8; 32],
        ledger_head_hash: [u8; 32],
        finalized_at: i64,
    ) -> Result<Self, ReceiptError> {
        let mid = market_id.into();
        let market_id_hash = Self::hash_market_id(&mid);
        let payout_numerators = Self::canonical_payout_numerators(final_outcome)?;
        let oracle_pubkey = signing_key.verifying_key().to_bytes();

        let mut receipt = Self {
            market_id: mid,
            market_id_hash,
            final_outcome,
            payout_numerators,
            payout_denominator: PAYOUT_DENOMINATOR,
            attestation_commitment,
            ledger_head_hash,
            finalized_at,
            oracle_pubkey,
            oracle_signature: Vec::new(),
        };

        let wire = receipt.to_wire_bytes();
        let sig = signing_key.sign(&wire);
        receipt.oracle_signature = sig.to_bytes().to_vec();

        Ok(receipt)
    }
}

/// Standalone verifier mimicking an on-chain Solana Anchor / EVM settlement contract precompile.
pub fn verify_onchain_receipt(receipt: &SettlementReceipt) -> Result<(), ReceiptError> {
    // 1. Verify market_id_hash matches SHA-256(market_id)
    let computed_mid_hash = SettlementReceipt::hash_market_id(&receipt.market_id);
    if computed_mid_hash != receipt.market_id_hash {
        return Err(ReceiptError::MarketIdHashMismatch {
            computed: hex::encode(computed_mid_hash),
            declared: hex::encode(receipt.market_id_hash),
        });
    }

    // 2. Verify payout conservation invariant & canonical outcome numerators
    let expected_numerators =
        SettlementReceipt::canonical_payout_numerators(receipt.final_outcome)?;
    let sum = receipt.payout_numerators[0].saturating_add(receipt.payout_numerators[1]);
    if receipt.payout_denominator != PAYOUT_DENOMINATOR
        || sum != PAYOUT_DENOMINATOR
        || receipt.payout_numerators != expected_numerators
    {
        return Err(ReceiptError::InvalidPayoutVector {
            outcome: receipt.final_outcome,
            numerators: receipt.payout_numerators,
            denominator: receipt.payout_denominator,
        });
    }

    // 3. Reconstruct exact 153-byte wire payload and verify Ed25519 signature
    let wire = receipt.to_wire_bytes();
    let vk = VerifyingKey::from_bytes(&receipt.oracle_pubkey)
        .map_err(|e| ReceiptError::InvalidPublicKey(e.to_string()))?;
    let sig = Signature::from_slice(&receipt.oracle_signature)
        .map_err(|e| ReceiptError::InvalidSignature(e.to_string()))?;

    vk.verify(&wire, &sig)
        .map_err(|e| ReceiptError::InvalidSignature(e.to_string()))?;

    Ok(())
}
