# Cryptographic Threat Model & Security Policy (`SECURITY.md`)

This document specifies the cryptographic trust boundaries, replay-resistance invariants, temporal/economic finality guarantees, and vulnerability disclosure policy for `resolution-service`.

---

## 1. Cryptographic Trust Boundaries (`src/provenance/`)

`resolution-service` separates **data provenance** (`AttestedPayload`) from **deterministic predicate evaluation** (`src/ast/` & `src/sports/`). A verifier (`verify_resolution_offline` or `MultiplexedSportsResolver::verify_report_offline`) never trusts an assertion of outcome without verifying the cryptographic commitment over the exact wire bytes.

### 1.1 Supported `AttestationEnvelope` Modes
| Envelope Variant | Cryptographic Root of Trust | Threat Model & Guarantees |
| :--- | :--- | :--- |
| **`Ed25519Authority`** | 32-byte Ed25519 verifying key (`ed25519-dalek`) held by the oracle authority node. | Proves that the designated oracle authority fetched `raw_bytes` from `source_url` at `fetch_timestamp` with `canonical_headers` and that zero bytes were mutated post-ingestion. Assumes the authority signing key is uncompromised. |
| **`NitroEnclave`** | AWS Nitro Enclave hardware attestation (`PCR0` enclave image hash, `PCR1` kernel hash, `PCR2` application hash) + enclave-generated Ed25519 signature. | Proves that the un-tampered binary Measurement (`PCR0`) inside an isolated hardware enclave executed the outbound TLS fetch and signed the commitment. Protects against host-operator tampering with MongoDB/memory state. |
| **`TlsNotarySession`** | Multiparty Computation (MPC) zkTLS Notary public key + server domain binding (`server_name`). | Proves that the TLS session record originated from a server presenting a certificate for `server_name` without revealing private session keys to the notary. |

### 1.2 Domain-Separated Payload Commitment Formula
Every `AttestedPayload` computes its 32-byte commitment hash using an explicit ASCII domain separator to prevent cross-protocol hash collisions:
$$\text{Commitment} = \text{SHA-256}\Big(\texttt{"RESOLUTION\_SERVICE\_ATTESTATION\_V1:"} \parallel \text{source\_url} \parallel \text{fetch\_timestamp}_{\text{BE8}} \parallel \text{SHA-256}(\text{raw\_bytes}) \parallel \text{canonical\_headers}\Big)$$

---

## 2. On-Chain Settlement Receipt Invariants (`src/receipt/`)

When a market transitions to `Finalized` or `Voided`, the clearinghouse issues a 153-byte `SettlementReceipt` designed for stateless verification inside Solana (`ed25519_program`) or EVM (`reportPayouts`) settlement contracts.

### 2.1 Replay & Cross-Market Substitution Protection
A valid Ed25519 signature over the 153-byte wire buffer (`SettlementReceipt::to_wire_bytes()`) cannot be replayed across markets, protocols, or historical states because the signed buffer binds:
1. **`[0..24]` `DOMAIN_SEPARATOR` (`"RAAS_ONCHAIN_RECEIPT_V1:"`):** Prevents signatures from other Ed25519 messages (e.g., `AttestedPayload` commitments) from validating as settlement receipts.
2. **`[24..56]` `market_id_hash` (`SHA-256(market_id)`):** Prevents an attacker from taking a valid `YES` receipt for Market A and submitting it to settle Market B.
3. **`[56..81]` `final_outcome` + `payout_numerators` + `payout_denominator`:** Enforces the conservation invariant $\texttt{numerators}[0] + \texttt{numerators}[1] \equiv 10{,}000$, preventing partial-drain or overflow exploits in downstream payout vaults.
4. **`[81..113]` `attestation_commitment`:** Binds the on-chain payout directly to the underlying HTTP wire payload commitment.
5. **`[113..145]` `ledger_head_hash`:** Binds the receipt to the exact sequential hash-chain state (`AuditLedger::head_hash()`) of the clearinghouse at the moment of finalization.

---

## 3. Arithmetic, Temporal & Sportsbook Edge-Case Mitigations

1. **Zero IEEE-754 Floating-Point Arithmetic (`src/ast/`):**
   All numeric extraction (`ParseDecimal`) and threshold comparisons operate on 128-bit fixed-point `rust_decimal::Decimal`. Inputs near binary floating-point boundaries (`99999.99999999999` vs `100000.0`) cannot trigger false settlements.
2. **Retroactive Source Revision Freeze (`src/fsm/`):**
   Once a market passes `cutoff_time + retroactive_freeze_seconds`, late upstream API revisions (e.g., an agency revising an economic print weeks later) are deterministically rejected.
3. **Multi-Snapshot VAR Confirmation Depth (`src/scheduler/`):**
   In-play sports predicates (`First20mGoal`, `OverUnder`, `AnytimeGoalscorer`) are never proposed on a single feed flash. Candidate outcomes are held in `TentativeObserved` and must persist across $\ge 2$ consecutive attested snapshots spanning $\Delta t \ge T_{\text{confirm}}$ (default `180s`) with consistent scoreboard totals (`confirmed_goals <= scoreboard_total`). If a goal is disallowed by VAR on a subsequent snapshot, the tracker emits `CandidateReverted` and preserves the `Active` market state.
4. **DNP / Unused Substitute Protection (`src/sports/predicates.rs`):**
   Player proposition markets verify roster participation (`Started`, `SubbedIn`, `DidNotPlay`). Unused bench players (`DidNotPlay`) deterministically settle to `VOID (3)` (`[5_000, 5_000]` 50/50 refund) rather than settling as a losing `NO (2)` bet.

---

## 4. Reporting a Vulnerability

If you discover a cryptographic, consensus, or parsing vulnerability:
- **Do not open a public GitHub issue.**
- Please report the finding privately via GitHub Security Advisories on this repository with a minimal reproduction test or payload vector.
