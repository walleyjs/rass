# Verifiable Resolution-as-a-Service (RaaS) Engine

An institutional-grade, cryptographically verifiable automated oracle and market resolution service designed for high-stakes prediction markets (e.g., Polymarket, Kalshi, SX Bet).

---

## Core Architectural Pillars

### 1. Cryptographic Data Provenance & Attestation (`src/provenance/`)
* **Byte-Level Verifiability:** Replaces naive, unverified outbound HTTPS requests with an `AttestedPayload` pipeline.
* **Canonical Cryptographic Commitment:** The engine generates and verifies a canonical SHA-256 commitment hash covering:
  $$\text{Commitment} = \text{SHA256}(\text{"RESOLUTION\_SERVICE\_ATTESTATION\_V1:"} \parallel \text{URL} \parallel \text{Timestamp} \parallel \text{Payload SHA256} \parallel \text{Canonical Headers})$$
* **Pluggable Attestation Envelopes:**
  * **zkTLS / TLSNotary Sessions:** Notarized MPC TLS session commitments ensuring the remote host served the exact byte stream.
  * **AWS Nitro Enclaves / TEE:** Hardware attestation with PCR measurements (`PCR0`, `PCR1`, `PCR2`) proving execution in a sealed enclave.
  * **Ed25519 Authority Signatures:** Hardware Security Module (HSM) signed attestations.

### 2. Deterministic, Float-Safe Assertion AST (`src/ast/`)
* **Zero Floating-Point Non-Determinism:** Implements exact 128-bit fixed-point decimal arithmetic (`rust_decimal`) and formal semantic versioning (`ParsedSemVer`) parsing.
* **Side-Effect-Free Pure AST Evaluator:**
  * `JsonPath`, `RegexMatch`, `DomSelector`, and `SchemaOrgJsonLd` extraction strategies.
  * Multi-witness consensus with configurable agreement ratio thresholds.
  * Type-safe binary operators (`GreaterThan`, `LessThanOrEqual`, `Equals`, etc.).

### 3. Finite State Machine (FSM) & Dispute Game Mechanics (`src/fsm/`)
* **Lifecycle State Transitions:**
  $$\text{Active} \xrightarrow{\text{Cutoff}} \text{Proposed} \xrightarrow{\text{Bond Challenge}} \text{Disputed} \xrightarrow{\text{Window Expire}} \text{Finalized}$$
* **Economic Security ($CoC > PfC$):** Bond escalation ladder ($100 \rightarrow 200 \rightarrow 400 \rightarrow 800\text{ USDC}$) preventing griefing attacks.
* **Retroactive Revision Freeze Windows:** Revisions after $\text{Cutoff} + T_{\text{freeze}}$ are strictly rejected with `FsmError::RetroactiveChangeFrozen`, preventing retrospective data manipulation.
* **Fail-Closed Circuit Breakers:** Protects against runaway disputes by freezing markets for governance review upon reaching maximum escalation rounds.

### 4. Institutional Verification & Hardening Suite (`tests/`)
* **Offline Proof Verifier (`src/verifier/`):** Third parties, smart contracts, or auditors can verify resolution correctness offline with zero network and zero database dependencies.
* **Property-Based Fuzzing (`tests/property_fuzz_tests.rs`):** Hundreds of automated `proptest` cases verifying that arbitrary strings, malformed JSON, and mutated payloads never panic the engine.
* **Historical Controversy Reproduction (`tests/historical_controversy_tests.rs`):**
  1. *Upstream Bureau Retroactive Revision:* Strict freeze-deadline rejection.
  2. *Mid-Flight API Schema Drift:* Fails closed with typed errors.
  3. *Multi-Witness Oracle Partition:* Rejects settlement when consensus drops below threshold.

---

## Quickstart

### Run the Demo Service
```bash
cargo run
```

### Run Full Test Suite & Property Fuzzing
```bash
cargo test
```
