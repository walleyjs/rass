use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::str::FromStr;

use crate::fsm::TemporalRules;

use super::fixture::{normalize_athlete_name, NormalizedFixtureState};
use super::predicates::{
    AthleteFilter, MatchSelection, OverUnderSide, ParticipationPolicy, SettlementScope,
    SportsMarketPredicate,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SportsMarketTemplate {
    pub market_id: String,
    pub template_key: String,
    pub title: String,
    pub question: String,
    pub canonical_rulebook: String,
    pub sport_path: String,
    pub event_id: String,
    pub predicate: SportsMarketPredicate,
    pub temporal_rules: TemporalRules,
    pub initial_poll_after: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FixtureMarketMatrix {
    pub event_id: String,
    pub sport_path: String,
    pub fixture_title: String,
    pub kickoff_timestamp: Option<i64>,
    pub home_team: String,
    pub away_team: String,
    pub templates: Vec<SportsMarketTemplate>,
}

fn sanitize_slug(input: &str) -> String {
    let s: String = normalize_athlete_name(input)
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    s.split('-')
        .filter(|p| !p.is_empty())
        .collect::<Vec<&str>>()
        .join("-")
}

impl FixtureMarketMatrix {
    /// Automatically expands a single NormalizedFixtureState into a complete, institutional-grade
    /// matrix of derivative sports markets with explicit scoping and participation void rules.
    pub fn generate_from_fixture(fixture: &NormalizedFixtureState) -> Self {
        let home_name = &fixture.home.team_name;
        let away_name = &fixture.away.team_name;
        let home_slug = sanitize_slug(home_name);
        let away_slug = sanitize_slug(away_name);
        let event_id = &fixture.event_id;
        let sport_path = &fixture.sport_path;

        let kickoff = fixture.kickoff_timestamp.unwrap_or(1_700_000_000);
        let full_time_poll = kickoff + 6000; // ~100m post-kickoff
        let in_play_poll = kickoff + 300; // 5m post-kickoff

        let default_temporal = TemporalRules {
            cutoff_time: kickoff,
            challenge_window_seconds: 1800,
            retroactive_freeze_seconds: 3600,
        };

        let is_soccer = sport_path.starts_with("soccer");
        let default_winner_scope = if is_soccer {
            SettlementScope::RegulationOnly
        } else {
            SettlementScope::IncludeOvertime
        };

        let scope_label = match default_winner_scope {
            SettlementScope::RegulationOnly => "in regulation time (90 minutes + injury time, excluding extra time and penalties)",
            SettlementScope::IncludeOvertime => "including overtime",
            SettlementScope::IncludeShootout => "including extra time and penalty shootouts",
        };

        let mut templates = Vec::new();

        // 1. Home Win
        templates.push(SportsMarketTemplate {
            market_id: format!("{}-{}-home-win", event_id, home_slug),
            template_key: "home_win".into(),
            title: format!("{} Win (Regulation)", home_name),
            question: format!("Will {} defeat {}?", home_name, away_name),
            canonical_rulebook: format!(
                "Resolves YES if {} outscores {} {}. Void if match is abandoned or postponed without completion.",
                home_name, away_name, scope_label
            ),
            sport_path: sport_path.clone(),
            event_id: event_id.clone(),
            predicate: SportsMarketPredicate::MatchOutcome {
                selection: MatchSelection::Home,
                scope: default_winner_scope,
            },
            temporal_rules: default_temporal.clone(),
            initial_poll_after: full_time_poll,
        });

        // 2. Away Win
        templates.push(SportsMarketTemplate {
            market_id: format!("{}-{}-away-win", event_id, away_slug),
            template_key: "away_win".into(),
            title: format!("{} Win (Regulation)", away_name),
            question: format!("Will {} defeat {}?", away_name, home_name),
            canonical_rulebook: format!(
                "Resolves YES if {} outscores {} {}. Void if match is abandoned or postponed without completion.",
                away_name, home_name, scope_label
            ),
            sport_path: sport_path.clone(),
            event_id: event_id.clone(),
            predicate: SportsMarketPredicate::MatchOutcome {
                selection: MatchSelection::Away,
                scope: default_winner_scope,
            },
            temporal_rules: default_temporal.clone(),
            initial_poll_after: full_time_poll,
        });

        if is_soccer {
            // 3. Regulation Draw
            templates.push(SportsMarketTemplate {
                market_id: format!("{}-{}-{}-draw", event_id, home_slug, away_slug),
                template_key: "draw".into(),
                title: "Match Draw (90m Regulation)".into(),
                question: format!(
                    "Will {} vs {} end in a regulation draw?",
                    home_name, away_name
                ),
                canonical_rulebook: format!(
                    "Resolves YES if {} and {} are tied at the end of 90 minutes regulation time (excluding extra time and penalty shootouts).",
                    home_name, away_name
                ),
                sport_path: sport_path.clone(),
                event_id: event_id.clone(),
                predicate: SportsMarketPredicate::MatchOutcome {
                    selection: MatchSelection::Draw,
                    scope: SettlementScope::RegulationOnly,
                },
                temporal_rules: default_temporal.clone(),
                initial_poll_after: full_time_poll,
            });

            // 4. Over 2.5 Goals (Regulation)
            templates.push(SportsMarketTemplate {
                market_id: format!("{}-over-2-5-goals", event_id),
                template_key: "over_under_2_5".into(),
                title: "Over 2.5 Total Goals (90m)".into(),
                question: format!(
                    "Will there be over 2.5 total regulation goals in {} vs {}?",
                    home_name, away_name
                ),
                canonical_rulebook: format!(
                    "Resolves YES if combined regulation goals in {} vs {} strictly exceed 2.5 (i.e. 3 or more goals). Extra time and shootout penalties are excluded.",
                    home_name, away_name
                ),
                sport_path: sport_path.clone(),
                event_id: event_id.clone(),
                predicate: SportsMarketPredicate::TotalScoreOverUnder {
                    threshold: Decimal::from_str("2.5").unwrap(),
                    side: OverUnderSide::Over,
                    scope: SettlementScope::RegulationOnly,
                },
                temporal_rules: default_temporal.clone(),
                initial_poll_after: in_play_poll,
            });

            // 5. Both Teams To Score (BTTS)
            templates.push(SportsMarketTemplate {
                market_id: format!("{}-btts", event_id),
                template_key: "both_teams_to_score".into(),
                title: "Both Teams To Score (BTTS)".into(),
                question: format!(
                    "Will both {} and {} score in regulation time?",
                    home_name, away_name
                ),
                canonical_rulebook: format!(
                    "Resolves YES if both {} and {} score at least 1 goal during 90m regulation time.",
                    home_name, away_name
                ),
                sport_path: sport_path.clone(),
                event_id: event_id.clone(),
                predicate: SportsMarketPredicate::BothTeamsToScore {
                    scope: SettlementScope::RegulationOnly,
                },
                temporal_rules: default_temporal.clone(),
                initial_poll_after: in_play_poll,
            });

            // 6. Goal in First 20 Minutes
            templates.push(SportsMarketTemplate {
                market_id: format!("{}-first-20m-goal", event_id),
                template_key: "first_20_min_goal".into(),
                title: "Goal in First 20 Minutes".into(),
                question: format!(
                    "Will a goal be scored in the first 20:00 of {} vs {}?",
                    home_name, away_name
                ),
                canonical_rulebook: format!(
                    "Resolves YES if a confirmed, non-disallowed goal is scored in Period 1 with match clock <= 20:00 in {} vs {}.",
                    home_name, away_name
                ),
                sport_path: sport_path.clone(),
                event_id: event_id.clone(),
                predicate: SportsMarketPredicate::GoalInTimeWindow {
                    period: 1,
                    max_base_minute: 20,
                    athlete_filter: None,
                },
                temporal_rules: default_temporal.clone(),
                initial_poll_after: in_play_poll,
            });

            // 7. Key Player Props (Attacking players / forwards discovered in roster)
            let mut home_attackers = 0;
            let mut away_attackers = 0;

            for ath in &fixture.athletes {
                let is_attacker = ath
                    .position
                    .as_deref()
                    .map(|p| {
                        let pl = p.to_lowercase();
                        pl.contains("forward")
                            || pl.contains("striker")
                            || pl.contains("attacker")
                            || pl.contains("wing")
                    })
                    .unwrap_or(false);

                if !is_attacker {
                    continue;
                }

                if ath.team_id == fixture.home.team_id {
                    if home_attackers >= 2 {
                        continue;
                    }
                    home_attackers += 1;
                } else if ath.team_id == fixture.away.team_id {
                    if away_attackers >= 2 {
                        continue;
                    }
                    away_attackers += 1;
                }

                let player_slug = sanitize_slug(&ath.display_name);
                let filter = AthleteFilter {
                    athlete_id: Some(ath.athlete_id.clone()),
                    athlete_name: ath.display_name.clone(),
                };

                // First Goal Scorer
                templates.push(SportsMarketTemplate {
                    market_id: format!("{}-first-goal-{}", event_id, player_slug),
                    template_key: format!("first_goal_scorer_{}", player_slug),
                    title: format!("{} First Goal Scorer", ath.display_name),
                    question: format!(
                        "Will {} score the first goal in {} vs {}?",
                        ath.display_name, home_name, away_name
                    ),
                    canonical_rulebook: format!(
                        "Resolves YES if {} scores the first non-own-goal of regulation time in {} vs {}. Resolves VOID if {} does not play or enters as a substitute after the first goal has already been scored.",
                        ath.display_name, home_name, away_name, ath.display_name
                    ),
                    sport_path: sport_path.clone(),
                    event_id: event_id.clone(),
                    predicate: SportsMarketPredicate::FirstScoringPlay {
                        athlete: filter.clone(),
                        participation_policy: ParticipationPolicy::MustEnterBeforeFirstGoal,
                        scope: SettlementScope::RegulationOnly,
                    },
                    temporal_rules: default_temporal.clone(),
                    initial_poll_after: in_play_poll,
                });

                // Player Anytime Goal
                templates.push(SportsMarketTemplate {
                    market_id: format!("{}-anytime-goal-{}", event_id, player_slug),
                    template_key: format!("player_anytime_{}", player_slug),
                    title: format!("{} Anytime Goal", ath.display_name),
                    question: format!(
                        "Will {} score anytime in {} vs {}?",
                        ath.display_name, home_name, away_name
                    ),
                    canonical_rulebook: format!(
                        "Resolves YES if {} scores at least 1 non-own-goal in regulation time in {} vs {}. Resolves VOID if {} does not participate (0 minutes played / DNP).",
                        ath.display_name, home_name, away_name, ath.display_name
                    ),
                    sport_path: sport_path.clone(),
                    event_id: event_id.clone(),
                    predicate: SportsMarketPredicate::PlayerScoringMilestone {
                        athlete: filter,
                        min_goals: 1,
                        participation_policy: ParticipationPolicy::MustPlay,
                        scope: SettlementScope::RegulationOnly,
                    },
                    temporal_rules: default_temporal.clone(),
                    initial_poll_after: in_play_poll,
                });
            }
        } else if sport_path.starts_with("basketball") {
            templates.push(SportsMarketTemplate {
                market_id: format!("{}-over-215-5-pts", event_id),
                template_key: "over_under_points".into(),
                title: "Over 215.5 Total Points".into(),
                question: format!(
                    "Will there be over 215.5 total points in {} vs {}?",
                    home_name, away_name
                ),
                canonical_rulebook: format!(
                    "Resolves YES if combined points in {} vs {} (including overtime) exceed 215.5.",
                    home_name, away_name
                ),
                sport_path: sport_path.clone(),
                event_id: event_id.clone(),
                predicate: SportsMarketPredicate::TotalScoreOverUnder {
                    threshold: Decimal::from_str("215.5").unwrap(),
                    side: OverUnderSide::Over,
                    scope: SettlementScope::IncludeOvertime,
                },
                temporal_rules: default_temporal,
                initial_poll_after: full_time_poll,
            });
        }

        Self {
            event_id: event_id.clone(),
            sport_path: sport_path.clone(),
            fixture_title: format!("{} vs {}", home_name, away_name),
            kickoff_timestamp: fixture.kickoff_timestamp,
            home_team: home_name.clone(),
            away_team: away_name.clone(),
            templates,
        }
    }
}
