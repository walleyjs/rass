use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Error, Debug, PartialEq, Eq, Clone)]
pub enum FsmError {
    #[error("Invalid state transition from {from} to {to}: {reason}")]
    InvalidTransition {
        from: String,
        to: String,
        reason: String,
    },
    #[error("Challenge window still active: current time {current_time} < expires at {expires_at}")]
    ChallengeWindowActive {
        current_time: i64,
        expires_at: i64,
    },
    #[error("Challenge window expired: current time {current_time} > expires at {expires_at}")]
    ChallengeWindowExpired {
        current_time: i64,
        expires_at: i64,
    },
    #[error("Insufficient bond: provided {provided} < required {required}")]
    InsufficientBond {
        provided: Decimal,
        required: Decimal,
    },
    #[error("Retroactive change rejected: revision at {revision_time} exceeds freeze window (cutoff {cutoff_time} + freeze {freeze_seconds}s)")]
    RetroactiveChangeFrozen {
        revision_time: i64,
        cutoff_time: i64,
        freeze_seconds: i64,
    },
    #[error("Circuit breaker tripped: {0}")]
    CircuitBreakerTripped(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bond {
    pub currency: String,
    pub amount: Decimal,
    pub bonded_party: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EscalationLadder {
    pub initial_bond: Decimal,
    pub multiplier: Decimal,
    pub max_rounds: u32,
}

impl Default for EscalationLadder {
    fn default() -> Self {
        Self {
            initial_bond: Decimal::from(100),
            multiplier: Decimal::from(2),
            max_rounds: 3,
        }
    }
}

impl EscalationLadder {
    pub fn required_bond_for_round(&self, round: u32) -> Decimal {
        let mut bond = self.initial_bond;
        for _ in 0..round {
            bond *= self.multiplier;
        }
        bond
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TemporalRules {
    pub cutoff_time: i64,
    pub challenge_window_seconds: i64,
    pub retroactive_freeze_seconds: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status")]
pub enum MarketLifecycleState {
    Active,
    Proposed {
        outcome: u8,
        proposed_at: i64,
        expires_at: i64,
        bond: Bond,
        assertion_hash: [u8; 32],
    },
    Disputed {
        round: u32,
        current_outcome: u8,
        counter_outcome: u8,
        total_bonded: Decimal,
        dispute_expires_at: i64,
        challenger: String,
    },
    Finalized {
        final_outcome: u8,
        finalized_at: i64,
        winning_party: String,
    },
    Voided {
        void_code: u8,
        reason: String,
        voided_at: i64,
    },
}

impl MarketLifecycleState {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Active => "Active",
            Self::Proposed { .. } => "Proposed",
            Self::Disputed { .. } => "Disputed",
            Self::Finalized { .. } => "Finalized",
            Self::Voided { .. } => "Voided",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolutionFsm {
    pub market_id: String,
    pub state: MarketLifecycleState,
    pub temporal_rules: TemporalRules,
    pub ladder: EscalationLadder,
    pub circuit_breaker_active: bool,
}

impl ResolutionFsm {
    pub fn new(
        market_id: impl Into<String>,
        temporal_rules: TemporalRules,
        ladder: EscalationLadder,
    ) -> Self {
        Self {
            market_id: market_id.into(),
            state: MarketLifecycleState::Active,
            temporal_rules,
            ladder,
            circuit_breaker_active: false,
        }
    }

    /// Propose an automated outcome backed by a valid cryptographic attestation
    pub fn propose(
        &mut self,
        outcome: u8,
        bond: Bond,
        assertion_hash: [u8; 32],
        current_time: i64,
    ) -> Result<(), FsmError> {
        if self.circuit_breaker_active {
            return Err(FsmError::CircuitBreakerTripped(
                "Cannot propose outcome while circuit breaker is active".into(),
            ));
        }

        match &self.state {
            MarketLifecycleState::Active => {
                if current_time < self.temporal_rules.cutoff_time {
                    return Err(FsmError::InvalidTransition {
                        from: self.state.name().into(),
                        to: "Proposed".into(),
                        reason: format!(
                            "Cutoff time not yet reached: {} < {}",
                            current_time, self.temporal_rules.cutoff_time
                        ),
                    });
                }

                let required_bond = self.ladder.initial_bond;
                if bond.amount < required_bond {
                    return Err(FsmError::InsufficientBond {
                        provided: bond.amount,
                        required: required_bond,
                    });
                }

                let expires_at = current_time + self.temporal_rules.challenge_window_seconds;
                self.state = MarketLifecycleState::Proposed {
                    outcome,
                    proposed_at: current_time,
                    expires_at,
                    bond,
                    assertion_hash,
                };
                Ok(())
            }
            other => Err(FsmError::InvalidTransition {
                from: other.name().into(),
                to: "Proposed".into(),
                reason: "Can only propose when market is in Active state".into(),
            }),
        }
    }

    /// Challenge a proposed outcome by posting an escalating bond
    pub fn challenge(
        &mut self,
        counter_outcome: u8,
        bond: Bond,
        current_time: i64,
    ) -> Result<(), FsmError> {
        match &self.state {
            MarketLifecycleState::Proposed {
                outcome,
                expires_at,
                bond: initial_bond,
                ..
            } => {
                if current_time > *expires_at {
                    return Err(FsmError::ChallengeWindowExpired {
                        current_time,
                        expires_at: *expires_at,
                    });
                }

                if *outcome == counter_outcome {
                    return Err(FsmError::InvalidTransition {
                        from: "Proposed".into(),
                        to: "Disputed".into(),
                        reason: "Counter outcome cannot equal proposed outcome".into(),
                    });
                }

                let required_bond = self.ladder.required_bond_for_round(1);
                if bond.amount < required_bond {
                    return Err(FsmError::InsufficientBond {
                        provided: bond.amount,
                        required: required_bond,
                    });
                }

                let total_bonded = initial_bond.amount + bond.amount;
                let dispute_expires_at =
                    current_time + self.temporal_rules.challenge_window_seconds;

                self.state = MarketLifecycleState::Disputed {
                    round: 1,
                    current_outcome: *outcome,
                    counter_outcome,
                    total_bonded,
                    dispute_expires_at,
                    challenger: bond.bonded_party,
                };
                Ok(())
            }
            MarketLifecycleState::Disputed {
                round,
                counter_outcome,
                total_bonded,
                dispute_expires_at,
                ..
            } => {
                if current_time > *dispute_expires_at {
                    return Err(FsmError::ChallengeWindowExpired {
                        current_time,
                        expires_at: *dispute_expires_at,
                    });
                }

                let next_round = round + 1;
                if next_round > self.ladder.max_rounds {
                    // Maximum escalation reached: trigger emergency council/void
                    self.circuit_breaker_active = true;
                    return Err(FsmError::CircuitBreakerTripped(
                        "Maximum escalation rounds exceeded: freezing market for governance council"
                            .into(),
                    ));
                }

                let required_bond = self.ladder.required_bond_for_round(next_round);
                if bond.amount < required_bond {
                    return Err(FsmError::InsufficientBond {
                        provided: bond.amount,
                        required: required_bond,
                    });
                }

                let new_total = *total_bonded + bond.amount;
                let new_expires_at = current_time + self.temporal_rules.challenge_window_seconds;

                self.state = MarketLifecycleState::Disputed {
                    round: next_round,
                    current_outcome: *counter_outcome,
                    counter_outcome: if *counter_outcome == 1 { 2 } else { 1 },
                    total_bonded: new_total,
                    dispute_expires_at: new_expires_at,
                    challenger: bond.bonded_party,
                };
                Ok(())
            }
            other => Err(FsmError::InvalidTransition {
                from: other.name().into(),
                to: "Disputed".into(),
                reason: "Can only challenge during Proposed or active Disputed states".into(),
            }),
        }
    }

    /// Finalize an undisputed proposal or settled dispute
    pub fn finalize(&mut self, current_time: i64) -> Result<u8, FsmError> {
        match &self.state {
            MarketLifecycleState::Proposed {
                outcome,
                expires_at,
                bond,
                ..
            } => {
                if current_time < *expires_at {
                    return Err(FsmError::ChallengeWindowActive {
                        current_time,
                        expires_at: *expires_at,
                    });
                }

                let out = *outcome;
                let winner = bond.bonded_party.clone();
                self.state = MarketLifecycleState::Finalized {
                    final_outcome: out,
                    finalized_at: current_time,
                    winning_party: winner,
                };
                Ok(out)
            }
            MarketLifecycleState::Disputed {
                counter_outcome,
                dispute_expires_at,
                challenger,
                ..
            } => {
                if current_time < *dispute_expires_at {
                    return Err(FsmError::ChallengeWindowActive {
                        current_time,
                        expires_at: *dispute_expires_at,
                    });
                }

                let out = *counter_outcome;
                let winner = challenger.clone();
                self.state = MarketLifecycleState::Finalized {
                    final_outcome: out,
                    finalized_at: current_time,
                    winning_party: winner,
                };
                Ok(out)
            }
            other => Err(FsmError::InvalidTransition {
                from: other.name().into(),
                to: "Finalized".into(),
                reason: "Can only finalize expired Proposed or expired Disputed state".into(),
            }),
        }
    }

    /// Emergency / Unresolvable Void transition
    pub fn void_market(&mut self, void_code: u8, reason: String, current_time: i64) -> Result<(), FsmError> {
        match &self.state {
            MarketLifecycleState::Finalized { .. } => Err(FsmError::InvalidTransition {
                from: "Finalized".into(),
                to: "Voided".into(),
                reason: "Cannot void a finalized market".into(),
            }),
            _ => {
                self.state = MarketLifecycleState::Voided {
                    void_code,
                    reason,
                    voided_at: current_time,
                };
                Ok(())
            }
        }
    }

    /// Evaluates whether an upstream data source revision is acceptable or violates freeze period
    pub fn evaluate_source_revision(
        &self,
        revision_timestamp: i64,
        source_revised_outcome: u8,
    ) -> Result<bool, FsmError> {
        let freeze_limit =
            self.temporal_rules.cutoff_time + self.temporal_rules.retroactive_freeze_seconds;

        if revision_timestamp > freeze_limit {
            return Err(FsmError::RetroactiveChangeFrozen {
                revision_time: revision_timestamp,
                cutoff_time: self.temporal_rules.cutoff_time,
                freeze_seconds: self.temporal_rules.retroactive_freeze_seconds,
            });
        }

        // Within freeze window: check if it alters current proposed state
        if let MarketLifecycleState::Proposed { outcome, .. } = &self.state {
            if *outcome != source_revised_outcome {
                // Legitimate revision before freeze limit that changes outcome
                return Ok(true);
            }
        }

        Ok(false)
    }
}
