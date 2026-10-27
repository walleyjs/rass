use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};

use crate::sports::SportsResolutionDecision;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TentativeCandidate {
    pub market_id: String,
    pub outcome_code: u8,
    pub explanation: String,
    pub first_seen_timestamp: i64,
    pub first_payload_commitment: [u8; 32],
    pub latest_seen_timestamp: i64,
    pub latest_payload_commitment: [u8; 32],
    pub confirmation_count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "confirmation_status")]
pub enum ConfirmationStepResult {
    /// Candidate outcome has survived the required confirmation depth window (or match is terminal)
    Confirmed {
        outcome_code: u8,
        explanation: String,
        first_seen_timestamp: i64,
        confirmed_at: i64,
        payload_commitment: [u8; 32],
    },
    /// First time seeing an in-play outcome trigger; held as tentative until confirmation depth elapses
    TentativeObserved {
        outcome_code: u8,
        first_seen_timestamp: i64,
        required_confirm_at: i64,
        next_poll_timestamp: i64,
    },
    /// Candidate is still active in-play, waiting for confirmation depth window to complete
    AwaitingConfirmationDepth {
        outcome_code: u8,
        first_seen_timestamp: i64,
        remaining_seconds: i64,
        next_poll_timestamp: i64,
    },
    /// Previously observed tentative candidate was rolled back (e.g., VAR overturned the goal)
    CandidateReverted {
        reverted_outcome_code: u8,
        reverted_at: i64,
        next_poll_timestamp: i64,
        reason: String,
    },
    /// Market definitively voided (e.g., DNP athlete or abandoned full-time market)
    ConfirmedVoid {
        code: u8,
        reason: String,
        payload_commitment: [u8; 32],
    },
    /// Market remains pending with no tentative candidate
    StillPending {
        next_poll_timestamp: i64,
        reason: String,
    },
}

/// Stateful multi-snapshot confirmation tracker that prevents premature in-play settlements
/// (such as goals overturned by VAR 90 seconds after initial scoreboard update).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConfirmationTracker {
    pub confirmation_depth_seconds: i64,
    pub candidates: HashMap<String, TentativeCandidate>,
}

impl ConfirmationTracker {
    pub fn new(confirmation_depth_seconds: i64) -> Self {
        Self {
            confirmation_depth_seconds,
            candidates: HashMap::new(),
        }
    }

    pub fn get_candidate(&self, market_id: &str) -> Option<&TentativeCandidate> {
        self.candidates.get(market_id)
    }

    pub fn process_decision(
        &mut self,
        market_id: &str,
        decision: &SportsResolutionDecision,
        is_in_play: bool,
        snapshot_timestamp: i64,
        payload_commitment: [u8; 32],
    ) -> ConfirmationStepResult {
        match decision {
            SportsResolutionDecision::Void { code, reason } => {
                self.candidates.remove(market_id);
                ConfirmationStepResult::ConfirmedVoid {
                    code: *code,
                    reason: reason.clone(),
                    payload_commitment,
                }
            }

            SportsResolutionDecision::Pending {
                next_poll_timestamp,
                reason,
            } => {
                if let Some(prev) = self.candidates.remove(market_id) {
                    ConfirmationStepResult::CandidateReverted {
                        reverted_outcome_code: prev.outcome_code,
                        reverted_at: snapshot_timestamp,
                        next_poll_timestamp: *next_poll_timestamp,
                        reason: format!(
                            "Tentative outcome {} reverted during confirmation window: {}",
                            prev.outcome_code, reason
                        ),
                    }
                } else {
                    ConfirmationStepResult::StillPending {
                        next_poll_timestamp: *next_poll_timestamp,
                        reason: reason.clone(),
                    }
                }
            }

            SportsResolutionDecision::ResolvedYes { explanation }
            | SportsResolutionDecision::ResolvedNo { explanation } => {
                let outcome_code = match decision {
                    SportsResolutionDecision::ResolvedYes { .. } => 1,
                    _ => 2,
                };

                // Terminal match states (CompletedRegulation, CompletedOvertime, Abandoned, etc.)
                // or 0-second depth settle immediately without waiting for in-play depth.
                if !is_in_play || self.confirmation_depth_seconds <= 0 {
                    let first_seen = self
                        .candidates
                        .remove(market_id)
                        .map(|c| c.first_seen_timestamp)
                        .unwrap_or(snapshot_timestamp);

                    return ConfirmationStepResult::Confirmed {
                        outcome_code,
                        explanation: explanation.clone(),
                        first_seen_timestamp: first_seen,
                        confirmed_at: snapshot_timestamp,
                        payload_commitment,
                    };
                }

                // In-play candidate: require persistence across confirmation_depth_seconds
                if let Some(existing) = self.candidates.get_mut(market_id) {
                    if existing.outcome_code == outcome_code {
                        existing.latest_seen_timestamp = snapshot_timestamp;
                        existing.latest_payload_commitment = payload_commitment;
                        existing.confirmation_count += 1;

                        let elapsed = snapshot_timestamp - existing.first_seen_timestamp;
                        if elapsed >= self.confirmation_depth_seconds {
                            let first_ts = existing.first_seen_timestamp;
                            self.candidates.remove(market_id);
                            return ConfirmationStepResult::Confirmed {
                                outcome_code,
                                explanation: explanation.clone(),
                                first_seen_timestamp: first_ts,
                                confirmed_at: snapshot_timestamp,
                                payload_commitment,
                            };
                        } else {
                            let remaining = self.confirmation_depth_seconds - elapsed;
                            let required_at =
                                existing.first_seen_timestamp + self.confirmation_depth_seconds;
                            return ConfirmationStepResult::AwaitingConfirmationDepth {
                                outcome_code,
                                first_seen_timestamp: existing.first_seen_timestamp,
                                remaining_seconds: remaining,
                                next_poll_timestamp: required_at,
                            };
                        }
                    }
                }

                // First observation of this candidate outcome in-play
                let required_confirm_at = snapshot_timestamp + self.confirmation_depth_seconds;
                self.candidates.insert(
                    market_id.to_string(),
                    TentativeCandidate {
                        market_id: market_id.to_string(),
                        outcome_code,
                        explanation: explanation.clone(),
                        first_seen_timestamp: snapshot_timestamp,
                        first_payload_commitment: payload_commitment,
                        latest_seen_timestamp: snapshot_timestamp,
                        latest_payload_commitment: payload_commitment,
                        confirmation_count: 1,
                    },
                );

                ConfirmationStepResult::TentativeObserved {
                    outcome_code,
                    first_seen_timestamp: snapshot_timestamp,
                    required_confirm_at,
                    next_poll_timestamp: required_confirm_at,
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoalescedOriginPoll {
    pub origin_key: String,
    pub earliest_poll_timestamp: i64,
    pub subscribed_markets: Vec<String>,
}

/// Coalesces polling across N derivative markets that share the same upstream resource
/// (`sport_path:event_id` or `primary_source_url`), scheduling the group at min(next_poll_timestamp).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CoalescedPollScheduler {
    market_origin: HashMap<String, String>,
    market_poll_at: HashMap<String, i64>,
    origin_markets: HashMap<String, BTreeSet<String>>,
}

impl CoalescedPollScheduler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn schedule_market(
        &mut self,
        market_id: impl Into<String>,
        origin_key: impl Into<String>,
        poll_timestamp: i64,
    ) {
        let mid = market_id.into();
        let orig = origin_key.into();

        self.market_origin.insert(mid.clone(), orig.clone());
        self.market_poll_at.insert(mid.clone(), poll_timestamp);
        self.origin_markets.entry(orig).or_default().insert(mid);
    }

    pub fn update_market_poll_horizon(&mut self, market_id: &str, next_poll_timestamp: i64) {
        if self.market_poll_at.contains_key(market_id) {
            self.market_poll_at
                .insert(market_id.to_string(), next_poll_timestamp);
        }
    }

    pub fn complete_market(&mut self, market_id: &str) {
        self.market_poll_at.remove(market_id);
        if let Some(orig) = self.market_origin.remove(market_id) {
            if let Some(set) = self.origin_markets.get_mut(&orig) {
                set.remove(market_id);
                if set.is_empty() {
                    self.origin_markets.remove(&orig);
                }
            }
        }
    }

    /// Returns all upstream origins whose earliest subscribed market horizon is `<= current_time`,
    /// ordered by earliest horizon first.
    pub fn due_origins(&self, current_time: i64) -> Vec<CoalescedOriginPoll> {
        let mut due = Vec::new();

        for (orig, markets) in &self.origin_markets {
            let mut min_ts: Option<i64> = None;
            let mut active_list = Vec::new();

            for m in markets {
                if let Some(&ts) = self.market_poll_at.get(m) {
                    min_ts = Some(min_ts.map_or(ts, |curr| curr.min(ts)));
                    active_list.push(m.clone());
                }
            }

            if let Some(earliest) = min_ts {
                if earliest <= current_time {
                    due.push(CoalescedOriginPoll {
                        origin_key: orig.clone(),
                        earliest_poll_timestamp: earliest,
                        subscribed_markets: active_list,
                    });
                }
            }
        }

        due.sort_by_key(|d| (d.earliest_poll_timestamp, d.origin_key.clone()));
        due
    }

    /// Returns the next scheduled poll horizon for an origin key, if any active markets remain.
    pub fn origin_horizon(&self, origin_key: &str) -> Option<i64> {
        self.origin_markets.get(origin_key).and_then(|markets| {
            markets
                .iter()
                .filter_map(|m| self.market_poll_at.get(m).copied())
                .min()
        })
    }
}
