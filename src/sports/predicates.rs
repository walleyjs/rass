use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use super::fixture::{
     normalize_athlete_name, ConfirmedScoringPlay, MatchLifecycleStatus, NormalizedFixtureState,
    ParticipationStatus,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SettlementScope {
    /// Strict regulation time only (e.g., 90 minutes + injury time in soccer; 4 quarters in NBA/NFL).
    /// Excludes extra time / overtime and penalty shootouts.
    RegulationOnly,
    /// Includes regulation time plus extra time / overtime periods, excluding penalty shootouts.
    IncludeOvertime,
    /// Includes regulation, overtime, and penalty shootouts (i.e., "To Advance / Tiebreaker Winner").
    IncludeShootout,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MatchSelection {
    Home,
    Away,
    Draw,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OverUnderSide {
    Over,
    Under,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AthleteFilter {
    pub athlete_id: Option<String>,
    pub athlete_name: String,
}

impl AthleteFilter {
    pub fn matches_play(&self, play: &ConfirmedScoringPlay) -> bool {
        if play.is_own_goal {
            return false;
        }
        if let (Some(target_id), Some(play_id)) = (&self.athlete_id, &play.primary_scorer_id) {
            if !target_id.is_empty() && target_id == play_id {
                return true;
            }
        }
        let target_norm = normalize_athlete_name(&self.athlete_name);
        if !target_norm.is_empty() {
            if let Some(scorer_norm) = &play.normalized_scorer_name {
                if scorer_norm.contains(&target_norm) || target_norm.contains(scorer_norm) {
                    return true;
                }
            }
        }
        false
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ParticipationPolicy {
    /// Void if the athlete is not in the starting lineup.
    MustStart,
    /// Void if the athlete plays 0 minutes (DNP / unused bench substitute).
    MustPlay,
    /// For First Goal Scorer: Void if the athlete is DNP or enters as a substitute after the first goal was already scored.
    MustEnterBeforeFirstGoal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "predicate_type", content = "params")]
pub enum SportsMarketPredicate {
    MatchOutcome {
        selection: MatchSelection,
        scope: SettlementScope,
    },
    ExactScoreline {
        target_home: u32,
        target_away: u32,
        scope: SettlementScope,
    },
    TotalScoreOverUnder {
        threshold: Decimal,
        side: OverUnderSide,
        scope: SettlementScope,
    },
    BothTeamsToScore {
        scope: SettlementScope,
    },
    GoalInTimeWindow {
        period: u32,
        max_base_minute: u32,
        athlete_filter: Option<AthleteFilter>,
    },
    FirstScoringPlay {
        athlete: AthleteFilter,
        participation_policy: ParticipationPolicy,
        scope: SettlementScope,
    },
    PlayerScoringMilestone {
        athlete: AthleteFilter,
        min_goals: u32,
        participation_policy: ParticipationPolicy,
        scope: SettlementScope,
    },
    CleanSheet {
        team_id: String,
        kept: bool,
        scope: SettlementScope,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision")]
pub enum SportsResolutionDecision {
    /// Market condition definitively satisfied (Outcome 1 = YES)
    ResolvedYes { explanation: String },
    /// Market condition definitively failed (Outcome 2 = NO)
    ResolvedNo { explanation: String },
    /// Market voided / annulled due to abandonment, postponement, or athlete DNP (Outcome 3 = VOID)
    Void { code: u8, reason: String },
    /// Market cannot yet be settled; provides intelligent next poll timestamp
    Pending {
        next_poll_timestamp: i64,
        reason: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SportsEvaluationConfig {
    /// Whether to allow irreversible milestones (e.g., Over 2.5 goals, BTTS, First 20m goal)
    /// to resolve while the match is still InPlay once VAR/consistency checks pass.
    pub allow_confirmed_in_play_settlement: bool,
    /// Minimum buffer in base minutes after a time window (e.g. 20m + 3m = 23m) before
    /// resolving NO on a time-window market while InPlay, protecting against late VAR checks.
    pub var_buffer_minutes: u32,
}

impl Default for SportsEvaluationConfig {
    fn default() -> Self {
        Self {
            allow_confirmed_in_play_settlement: true,
            var_buffer_minutes: 3,
        }
    }
}

impl SportsMarketPredicate {
    /// Returns true if this predicate can only ever be resolved at match completion
    pub fn is_terminal_only(&self) -> bool {
        matches!(
            self,
            Self::MatchOutcome { .. } | Self::ExactScoreline { .. }
        )
    }

    /// Evaluates the sports predicate against a normalized fixture state.
    pub fn evaluate(
        &self,
        fixture: &NormalizedFixtureState,
        current_time: i64,
        config: &SportsEvaluationConfig,
    ) -> SportsResolutionDecision {
        // 1. Pre-Match Kickoff Horizon Optimization
        if fixture.lifecycle_status == MatchLifecycleStatus::PreMatch {
            let kickoff = fixture.kickoff_timestamp.unwrap_or(current_time + 3600);
            let next_poll = if kickoff > current_time {
                if self.is_terminal_only() {
                    kickoff + 6000 // Poll ~100 mins after kickoff for full-time markets
                } else {
                    kickoff + 300 // Poll 5 mins after kickoff for early/in-play markets
                }
            } else {
                current_time + 300
            };

            return SportsResolutionDecision::Pending {
                next_poll_timestamp: next_poll,
                reason: "Fixture is in PreMatch state".into(),
            };
        }

        // 2. Check VAR / Scoreboard-Play Ledger Consistency Guard
        // If the scoreboard contradicts the play ledger or an active VAR review is flagged,
        // hold all resolutions in Pending until the feed reconciles.
        if !fixture.scoreboard_play_ledger_consistent || fixture.has_unconfirmed_or_var_review_play
        {
            return SportsResolutionDecision::Pending {
                next_poll_timestamp: current_time + 120,
                reason: "Holding settlement: active VAR review or scoreboard-to-play-ledger desync detected".into(),
            };
        }

        let is_completed = fixture.lifecycle_status.is_terminal_completed();
        let is_aborted = fixture.lifecycle_status.is_terminal_aborted();
        let is_in_play = fixture.lifecycle_status == MatchLifecycleStatus::InPlay;

        // Helper to extract scoped scores (Home, Away)
        let scoped_scores = |scope: SettlementScope| -> Result<(u32, u32), String> {
            match scope {
                SettlementScope::RegulationOnly => {
                    if matches!(
                        fixture.lifecycle_status,
                        MatchLifecycleStatus::CompletedOvertime
                            | MatchLifecycleStatus::CompletedShootout
                    ) && (!fixture.home.has_explicit_period_breakdown
                        || !fixture.away.has_explicit_period_breakdown)
                    {
                        return Err(
                            "Fixture finished in OT/Shootout without period linescore breakdown"
                                .into(),
                        );
                    }
                    Ok((fixture.home.regulation_score, fixture.away.regulation_score))
                }
                SettlementScope::IncludeOvertime | SettlementScope::IncludeShootout => Ok((
                    fixture.home.regulation_score + fixture.home.overtime_score,
                    fixture.away.regulation_score + fixture.away.overtime_score,
                )),
            }
        };

        // Helper to filter confirmed scoring plays by SettlementScope
        let scoped_plays = |scope: SettlementScope| -> Vec<&ConfirmedScoringPlay> {
            fixture
                .confirmed_scoring_plays
                .iter()
                .filter(|p| match scope {
                    SettlementScope::RegulationOnly => p.clock.period <= fixture.regulation_periods,
                    SettlementScope::IncludeOvertime | SettlementScope::IncludeShootout => {
                        p.clock.period <= fixture.regulation_periods + 2
                    }
                })
                .collect()
        };

        match self {
            Self::MatchOutcome { selection, scope } => {
                if is_aborted {
                    return SportsResolutionDecision::Void {
                        code: 3,
                        reason: format!(
                            "Fixture {:?} before completion; MatchOutcome voided",
                            fixture.lifecycle_status
                        ),
                    };
                }
                if !is_completed {
                    return SportsResolutionDecision::Pending {
                        next_poll_timestamp: current_time + 300,
                        reason: "Match still in progress".into(),
                    };
                }

                let (h_score, a_score) = match scoped_scores(*scope) {
                    Ok(s) => s,
                    Err(reason) => return SportsResolutionDecision::Void { code: 3, reason },
                };

                let won = if *scope == SettlementScope::IncludeShootout && h_score == a_score {
                    match (fixture.home.shootout_score, fixture.away.shootout_score) {
                        (Some(h_pen), Some(a_pen)) => match selection {
                            MatchSelection::Home => h_pen > a_pen,
                            MatchSelection::Away => a_pen > h_pen,
                            MatchSelection::Draw => h_pen == a_pen,
                        },
                        _ => match selection {
                            MatchSelection::Home => h_score > a_score,
                            MatchSelection::Away => a_score > h_score,
                            MatchSelection::Draw => h_score == a_score,
                        },
                    }
                } else {
                    match selection {
                        MatchSelection::Home => h_score > a_score,
                        MatchSelection::Away => a_score > h_score,
                        MatchSelection::Draw => h_score == a_score,
                    }
                };

                if won {
                    SportsResolutionDecision::ResolvedYes {
                        explanation: format!(
                            "MatchOutcome {:?} ({:?}) satisfied with score {}-{}",
                            selection, scope, h_score, a_score
                        ),
                    }
                } else {
                    SportsResolutionDecision::ResolvedNo {
                        explanation: format!(
                            "MatchOutcome {:?} ({:?}) not met with score {}-{}",
                            selection, scope, h_score, a_score
                        ),
                    }
                }
            }

            Self::ExactScoreline {
                target_home,
                target_away,
                scope,
            } => {
                if is_aborted {
                    return SportsResolutionDecision::Void {
                        code: 3,
                        reason: format!(
                            "Fixture {:?} before completion; ExactScoreline voided",
                            fixture.lifecycle_status
                        ),
                    };
                }
                if !is_completed {
                    return SportsResolutionDecision::Pending {
                        next_poll_timestamp: current_time + 300,
                        reason: "Match still in progress".into(),
                    };
                }

                let (h_score, a_score) = match scoped_scores(*scope) {
                    Ok(s) => s,
                    Err(reason) => return SportsResolutionDecision::Void { code: 3, reason },
                };

                if h_score == *target_home && a_score == *target_away {
                    SportsResolutionDecision::ResolvedYes {
                        explanation: format!("Exact scoreline {}-{} matched", h_score, a_score),
                    }
                } else {
                    SportsResolutionDecision::ResolvedNo {
                        explanation: format!(
                            "Final scoped score {}-{} did not match target {}-{}",
                            h_score, a_score, target_home, target_away
                        ),
                    }
                }
            }

            Self::TotalScoreOverUnder {
                threshold,
                side,
                scope,
            } => {
                let (h_score, a_score) = match scoped_scores(*scope) {
                    Ok(s) => s,
                    Err(reason) => return SportsResolutionDecision::Void { code: 3, reason },
                };
                let total = Decimal::from(h_score + a_score);

                // Monotonic early resolution check (if total already strictly exceeds threshold)
                if total > *threshold {
                    if is_completed
                        || is_aborted
                        || (is_in_play && config.allow_confirmed_in_play_settlement)
                    {
                        return match side {
                            OverUnderSide::Over => SportsResolutionDecision::ResolvedYes {
                                explanation: format!(
                                    "Total score {} exceeded threshold {}",
                                    total, threshold
                                ),
                            },
                            OverUnderSide::Under => SportsResolutionDecision::ResolvedNo {
                                explanation: format!(
                                    "Total score {} exceeded under threshold {}",
                                    total, threshold
                                ),
                            },
                        };
                    }
                }

                if is_aborted {
                    return SportsResolutionDecision::Void {
                        code: 3,
                        reason: format!(
                            "Fixture {:?} at score {}-{} before Over/Under threshold {} was crossed",
                            fixture.lifecycle_status, h_score, a_score, threshold
                        ),
                    };
                }

                if is_completed {
                    return if total > *threshold {
                        match side {
                            OverUnderSide::Over => SportsResolutionDecision::ResolvedYes {
                                explanation: format!("Final total {} > {}", total, threshold),
                            },
                            OverUnderSide::Under => SportsResolutionDecision::ResolvedNo {
                                explanation: format!("Final total {} > {}", total, threshold),
                            },
                        }
                    } else if total < *threshold {
                        match side {
                            OverUnderSide::Over => SportsResolutionDecision::ResolvedNo {
                                explanation: format!("Final total {} < {}", total, threshold),
                            },
                            OverUnderSide::Under => SportsResolutionDecision::ResolvedYes {
                                explanation: format!("Final total {} < {}", total, threshold),
                            },
                        }
                    } else {
                        // Exact integer push (e.g. Over 2.0 when total == 2)
                        SportsResolutionDecision::Void {
                            code: 3,
                            reason: format!("Exact total push at {}", total),
                        }
                    };
                }

                SportsResolutionDecision::Pending {
                    next_poll_timestamp: current_time + 300,
                    reason: format!("Total score currently {} vs threshold {}", total, threshold),
                }
            }

            Self::BothTeamsToScore { scope } => {
                let (h_score, a_score) = match scoped_scores(*scope) {
                    Ok(s) => s,
                    Err(reason) => return SportsResolutionDecision::Void { code: 3, reason },
                };

                if h_score > 0 && a_score > 0 {
                    if is_completed
                        || is_aborted
                        || (is_in_play && config.allow_confirmed_in_play_settlement)
                    {
                        return SportsResolutionDecision::ResolvedYes {
                            explanation: format!(
                                "Both teams scored (scoped score {}-{})",
                                h_score, a_score
                            ),
                        };
                    }
                }

                if is_aborted {
                    return SportsResolutionDecision::Void {
                        code: 3,
                        reason: format!(
                            "Fixture {:?} at {}-{} before both teams scored",
                            fixture.lifecycle_status, h_score, a_score
                        ),
                    };
                }

                if is_completed {
                    return SportsResolutionDecision::ResolvedNo {
                        explanation: format!(
                            "Match finished {}-{}; both teams did not score",
                            h_score, a_score
                        ),
                    };
                }

                SportsResolutionDecision::Pending {
                    next_poll_timestamp: current_time + 300,
                    reason: "Waiting for both teams to score or match completion".into(),
                }
            }

            Self::GoalInTimeWindow {
                period,
                max_base_minute,
                athlete_filter,
            } => {
                let matching_play = fixture.confirmed_scoring_plays.iter().find(|p| {
                    let in_window = p.clock.period == *period
                        && p.clock.base_minute <= *max_base_minute
                        && p.clock.stoppage_minute == 0
                        && p.clock.elapsed_seconds_in_period <= max_base_minute * 60;
                    if !in_window {
                        return false;
                    }
                    match athlete_filter {
                        Some(af) => af.matches_play(p),
                        None => true,
                    }
                });

                if let Some(play) = matching_play {
                    if is_completed
                        || is_aborted
                        || (is_in_play && config.allow_confirmed_in_play_settlement)
                    {
                        return SportsResolutionDecision::ResolvedYes {
                            explanation: format!(
                                "Qualifying goal scored in period {} at minute {}: '{}'",
                                play.clock.period, play.clock.base_minute, play.description
                            ),
                        };
                    }
                }

                // Determine if the time window has conclusively passed (including VAR buffer if in-play)
                let window_passed_in_play = fixture.current_clock.period > *period
                    || (fixture.current_clock.period == *period
                        && fixture.current_clock.base_minute
                            >= max_base_minute + config.var_buffer_minutes);

                if is_completed || (is_in_play && window_passed_in_play && config.allow_confirmed_in_play_settlement) || (is_aborted && window_passed_in_play) {
                    return SportsResolutionDecision::ResolvedNo {
                        explanation: format!(
                            "Time window (Period {}, 0..={}m) elapsed with no qualifying goal",
                            period, max_base_minute
                        ),
                    };
                }

                if is_aborted {
                    return SportsResolutionDecision::Void {
                        code: 3,
                        reason: "Match abandoned before time window elapsed".into(),
                    };
                }

                SportsResolutionDecision::Pending {
                    next_poll_timestamp: current_time + 180,
                    reason: format!(
                        "Time window (Period {}, <= {}m) still active or within VAR buffer",
                        period, max_base_minute
                    ),
                }
            }

            Self::FirstScoringPlay {
                athlete,
                participation_policy,
                scope,
            } => {
                let plays = scoped_plays(*scope);
                let first_non_og = plays.iter().find(|p| !p.is_own_goal).copied();

                // Check athlete participation status from roster
                let roster_entry =
                    fixture.find_athlete(athlete.athlete_id.as_deref(), Some(&athlete.athlete_name));

                if let Some(entry) = roster_entry {
                    match participation_policy {
                        ParticipationPolicy::MustStart => {
                            if entry.status != ParticipationStatus::Started {
                                return SportsResolutionDecision::Void {
                                    code: 3,
                                    reason: format!(
                                        "Athlete '{}' did not start ({:?}); voided per MustStart policy",
                                        entry.display_name, entry.status
                                    ),
                                };
                            }
                        }
                        ParticipationPolicy::MustPlay => {
                            if is_completed && entry.status == ParticipationStatus::DidNotPlay {
                                return SportsResolutionDecision::Void {
                                    code: 3,
                                    reason: format!(
                                        "Athlete '{}' did not play (DNP); voided per MustPlay policy",
                                        entry.display_name
                                    ),
                                };
                            }
                        }
                        ParticipationPolicy::MustEnterBeforeFirstGoal => {
                            match entry.status {
                                ParticipationStatus::DidNotPlay => {
                                    if first_non_og.is_some() || is_completed {
                                        return SportsResolutionDecision::Void {
                                            code: 3,
                                            reason: format!(
                                                "Athlete '{}' was not on the pitch when first goal was scored (or DNP)",
                                                entry.display_name
                                            ),
                                        };
                                    }
                                }
                                ParticipationStatus::SubbedIn {
                                    minute: Some(sub_min),
                                } => {
                                    if let Some(fg) = first_non_og {
                                        if sub_min > fg.clock.base_minute {
                                            return SportsResolutionDecision::Void {
                                                code: 3,
                                                reason: format!(
                                                    "Athlete '{}' subbed in at {}m after first goal at {}m",
                                                    entry.display_name, sub_min, fg.clock.base_minute
                                                ),
                                            };
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }

                if let Some(first_goal) = first_non_og {
                    if is_completed
                        || is_aborted
                        || (is_in_play && config.allow_confirmed_in_play_settlement)
                    {
                        if athlete.matches_play(first_goal) {
                            return SportsResolutionDecision::ResolvedYes {
                                explanation: format!(
                                    "Athlete '{}' scored first goal at period {} minute {}",
                                    athlete.athlete_name,
                                    first_goal.clock.period,
                                    first_goal.clock.base_minute
                                ),
                            };
                        } else {
                            return SportsResolutionDecision::ResolvedNo {
                                explanation: format!(
                                    "First goal was scored by another player ({:?})",
                                    first_goal.primary_scorer_name
                                ),
                            };
                        }
                    }
                }

                if is_aborted {
                    return SportsResolutionDecision::Void {
                        code: 3,
                        reason: "Match abandoned before any goal was scored".into(),
                    };
                }

                if is_completed {
                    return SportsResolutionDecision::ResolvedNo {
                        explanation: "Match finished with 0 valid goals".into(),
                    };
                }

                SportsResolutionDecision::Pending {
                    next_poll_timestamp: current_time + 300,
                    reason: "No goal scored yet".into(),
                }
            }

            Self::PlayerScoringMilestone {
                athlete,
                min_goals,
                participation_policy,
                scope,
            } => {
                let roster_entry =
                    fixture.find_athlete(athlete.athlete_id.as_deref(), Some(&athlete.athlete_name));

                if let Some(entry) = roster_entry {
                    if *participation_policy == ParticipationPolicy::MustStart
                        && entry.status != ParticipationStatus::Started
                    {
                        return SportsResolutionDecision::Void {
                            code: 3,
                            reason: format!(
                                "Athlete '{}' did not start; voided per MustStart policy",
                                entry.display_name
                            ),
                        };
                    }
                    if is_completed && entry.status == ParticipationStatus::DidNotPlay {
                        return SportsResolutionDecision::Void {
                            code: 3,
                            reason: format!(
                                "Athlete '{}' did not participate (DNP); voided per MustPlay policy",
                                entry.display_name
                            ),
                        };
                    }
                }

                let plays = scoped_plays(*scope);
                let player_goals = plays
                    .iter()
                    .filter(|p| athlete.matches_play(p))
                    .count() as u32;

                if player_goals >= *min_goals {
                    if is_completed
                        || is_aborted
                        || (is_in_play && config.allow_confirmed_in_play_settlement)
                    {
                        return SportsResolutionDecision::ResolvedYes {
                            explanation: format!(
                                "Athlete '{}' scored {} goals (required >= {})",
                                athlete.athlete_name, player_goals, min_goals
                            ),
                        };
                    }
                }

                if is_aborted {
                    return SportsResolutionDecision::Void {
                        code: 3,
                        reason: "Match abandoned before athlete reached scoring milestone".into(),
                    };
                }

                if is_completed {
                    return SportsResolutionDecision::ResolvedNo {
                        explanation: format!(
                            "Match finished; athlete '{}' scored {} goals (< {})",
                            athlete.athlete_name, player_goals, min_goals
                        ),
                    };
                }

                SportsResolutionDecision::Pending {
                    next_poll_timestamp: current_time + 300,
                    reason: format!(
                        "Athlete '{}' currently has {} goals (target {})",
                        athlete.athlete_name, player_goals, min_goals
                    ),
                }
            }

            Self::CleanSheet {
                team_id,
                kept,
                scope,
            } => {
                let (h_score, a_score) = match scoped_scores(*scope) {
                    Ok(s) => s,
                    Err(reason) => return SportsResolutionDecision::Void { code: 3, reason },
                };

                let opponent_score = if team_id == &fixture.home.team_id {
                    a_score
                } else {
                    h_score
                };

                if opponent_score > 0 {
                    if is_completed
                        || is_aborted
                        || (is_in_play && config.allow_confirmed_in_play_settlement)
                    {
                        return if *kept {
                            SportsResolutionDecision::ResolvedNo {
                                explanation: format!(
                                    "Opponent scored {} goals; clean sheet lost",
                                    opponent_score
                                ),
                            }
                        } else {
                            SportsResolutionDecision::ResolvedYes {
                                explanation: format!(
                                    "Opponent scored {} goals; clean sheet lost condition met",
                                    opponent_score
                                ),
                            }
                        };
                    }
                }

                if is_aborted {
                    return SportsResolutionDecision::Void {
                        code: 3,
                        reason: "Match abandoned with 0 opponent goals; clean sheet market voided"
                            .into(),
                    };
                }

                if is_completed {
                    return if *kept {
                        SportsResolutionDecision::ResolvedYes {
                            explanation: "Match finished with 0 opponent goals; clean sheet kept"
                                .into(),
                        }
                    } else {
                        SportsResolutionDecision::ResolvedNo {
                            explanation: "Match finished with 0 opponent goals; clean sheet kept"
                                .into(),
                        }
                    };
                }

                SportsResolutionDecision::Pending {
                    next_poll_timestamp: current_time + 300,
                    reason: "Clean sheet intact while match is in progress".into(),
                }
            }
        }
    }
}
