# Verifiable Resolution-as-a-Service (RaaS) Engine

An institutional-grade, cryptographically verifiable automated oracle and market resolution service designed for high-stakes prediction markets (e.g., Polymarket, Kalshi, SX Bet).

---

## Core Architectural Pillars

### 1. Cryptographic Data Provenance & Attestation (`src/provenance/`)
* **Byte-Level Verifiability:** Replaces naive, unverified outbound HTTPS requests with an `AttestedPayload` pipeline (`HttpAttestingProvider` with rate-pacing & deduplication caching in `src/provenance/http.rs`).
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

### 3. Multiplexed Sports Fixture Matrix & Resolution Engine (`src/sports/`)
* **1-Attestation-to-$N$-Markets Multiplexed Resolution (`MultiplexedSportsResolver`):** Verifies the cryptographic signature of an ESPN Summary payload once, normalizes the fixture state once, and resolves an entire matrix of derivative markets (`MatchOutcome`, `TotalScoreOverUnder`, `BothTeamsToScore`, `GoalInTimeWindow`, `FirstScoringPlay`, `PlayerScoringMilestone`, `CleanSheet`) in a single atomic pass with a Merkle commitment over all verdicts.
* **Solves the 5 Institutional Sportsbook Failure Modes:**
  1. **Stoppage-Time Clock Separation (`MatchClock`):** Parses `"45'+4'"` into `(period: 1, base_minute: 45, stoppage_minute: 4)` rather than concatenating digits into `454`, and enforces strict chronological ordering across period boundaries.
  2. **Period Scoping (`RegulationOnly` vs. `IncludeOvertime` vs. `IncludeShootout`):** Extracts period-by-period linescores so 90-minute soccer regulation markets (`Draw`, `Over/Under 2.5`) are never corrupted by Extra Time (`AET`) or Penalty Shootout goals.
  3. **Athlete Participation Void Policies (`MustStart`, `MustPlay`, `MustEnterBeforeFirstGoal`):** Automatically voids player prop markets (`Outcome 3 = VOID`) when an athlete is DNP (0 minutes played) or enters as a substitute after the first goal has already been scored.
  4. **VAR & Scoreboard-to-Play-Ledger Consistency Guards:** Holds in-play settlements in `Pending` whenever an active VAR review is detected or the scoreboard tally disagrees with confirmed scoring plays.
  5. **Abandoned & Postponed Match Scoping:** Voids undetermined full-time markets when a fixture is `Abandoned` or `Postponed`, while preserving early time-window or milestone markets that were already unconditionally completed prior to abandonment.

### 4. Finite State Machine (FSM) & Dispute Game Mechanics (`src/fsm/`)
* **Lifecycle State Transitions:**
  $$\text{Active} \xrightarrow{\text{Cutoff}} \text{Proposed} \xrightarrow{\text{Bond Challenge}} \text{Disputed} \xrightarrow{\text{Window Expire}} \text{Finalized}$$
* **Economic Security ($CoC > PfC$):** Bond escalation ladder ($100 \rightarrow 200 \rightarrow 400 \rightarrow 800\text{ USDC}$) preventing griefing attacks.
* **Retroactive Revision Freeze Windows:** Revisions after $\text{Cutoff} + T_{\text{freeze}}$ are strictly rejected with `FsmError::RetroactiveChangeFrozen`, preventing retrospective data manipulation.

### 5. Institutional Verification & Hardening Suite (`tests/`)
* **Offline Proof Verifiers (`src/verifier/` & `MultiplexedSportsResolver::verify_report_offline`):** Third parties, smart contracts, or auditors can verify single-market and multiplexed sports matrix resolutions offline with zero network and zero database dependencies.
* **Property-Based Fuzzing (`tests/property_fuzz_tests.rs` & `tests/sports_resolution_tests.rs`):** Automated `proptest` suites verifying that arbitrary strings, malformed JSON, stoppage-time clocks, and mutated payloads never panic the engine.
* **Historical Controversy & Sports Failure Regression Suites (`tests/historical_controversy_tests.rs` & `tests/sports_resolution_tests.rs`).**

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
