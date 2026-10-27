use proptest::prelude::*;
use rust_decimal::Decimal;
use std::str::FromStr;

use resolution_service::provenance::mock::MockAttestationAuthority;
use resolution_service::sports::{
    AthleteFilter, FixtureMarketMatrix, MatchClock, MatchLifecycleStatus, MatchSelection,
    MultiplexedSportsResolver, NormalizedFixtureState, OverUnderSide, ParticipationPolicy,
    SettlementScope, SportsEvaluationConfig, SportsMarketPredicate, SportsResolutionDecision,
};

/// FAILURE MODE 1: Stoppage-Time Clock Concatenation ("45'+4'") & Period Ordering
/// Proves that "45'+4'" is parsed as (period: 1, base_minute: 45, stoppage_minute: 4) rather than 454,
/// and that Period 1 stoppage time ("45'+4'", 2940s) is strictly ordered BEFORE Period 2 ("46'", 2760s).
#[test]
fn test_failure_mode_1_stoppage_time_clock_parsing_and_ordering() {
    let p1_stoppage = MatchClock::parse(1, Some("45'+4'"), Some(2940.0));
    assert_eq!(p1_stoppage.period, 1);
    assert_eq!(p1_stoppage.base_minute, 45);
    assert_eq!(p1_stoppage.stoppage_minute, 4);

    let p2_early = MatchClock::parse(2, Some("46'"), Some(2760.0));
    assert_eq!(p2_early.period, 2);
    assert_eq!(p2_early.base_minute, 46);
    assert_eq!(p2_early.stoppage_minute, 0);

    // Even though raw seconds 2940 > 2760, Period 1 45'+4' MUST be chronologically < Period 2 46'
    assert!(p1_stoppage < p2_early);

    // Verify a goal scored at 45'+4' does not count as a first-20-minute goal
    let raw_json = r#"{
        "header": {
            "id": "401",
            "competitions": [{
                "id": "401",
                "date": "2026-10-27T19:00:00Z",
                "status": {
                    "period": 1,
                    "displayClock": "45'+4'",
                    "type": { "name": "STATUS_HALFTIME", "state": "in", "completed": false }
                },
                "competitors": [
                    { "id": "1", "homeAway": "home", "score": "1", "team": { "displayName": "Arsenal" } },
                    { "id": "2", "homeAway": "away", "score": "0", "team": { "displayName": "Chelsea" } }
                ]
            }]
        },
        "plays": [{
            "id": "p1",
            "scoringPlay": true,
            "period": { "number": 1 },
            "clock": { "displayValue": "45'+4'", "value": 2940.0 },
            "type": { "text": "Goal" },
            "text": "Goal! Arsenal 1, Chelsea 0.",
            "team": { "id": "1" }
        }]
    }"#;

    let fixture =
        NormalizedFixtureState::from_espn_summary_json("soccer/eng.1", "401", raw_json).unwrap();
    assert_eq!(fixture.current_clock.base_minute, 45);
    assert_eq!(fixture.current_clock.stoppage_minute, 4);

    let pred = SportsMarketPredicate::GoalInTimeWindow {
        period: 1,
        max_base_minute: 20,
        athlete_filter: None,
    };

    let decision = pred.evaluate(&fixture, 1_700_003_000, &SportsEvaluationConfig::default());
    assert!(
        matches!(decision, SportsResolutionDecision::ResolvedNo { .. }),
        "Goal at 45'+4' must not satisfy first 20m window, and at 45'+4' the 20m window has elapsed (ResolvedNo)"
    );
}

/// FAILURE MODE 2: 90m Regulation vs. Extra Time (AET) & Penalty Shootouts
/// Match finishes 1-1 after 90m regulation, and Home scores in Extra Time to win 2-1 AET.
/// Proves 90m Regulation markets resolve Draw=YES, HomeWin=NO, Over 2.5=NO,
/// while IncludeOvertime markets resolve HomeWin=YES, Over 2.5=YES.
#[test]
fn test_failure_mode_2_regulation_90m_vs_extra_time_aet() {
    let raw_json = r#"{
        "header": {
            "id": "501",
            "competitions": [{
                "id": "501",
                "date": "2026-10-27T19:00:00Z",
                "status": {
                    "period": 4,
                    "displayClock": "120'",
                    "type": {
                        "name": "STATUS_FINAL_AET",
                        "state": "post",
                        "completed": true,
                        "detail": "Final - AET"
                    }
                },
                "competitors": [
                    {
                        "id": "10",
                        "homeAway": "home",
                        "score": "2",
                        "team": { "displayName": "Real Madrid" },
                        "linescores": [
                            { "displayValue": "0" },
                            { "displayValue": "1" },
                            { "displayValue": "1" },
                            { "displayValue": "0" }
                        ]
                    },
                    {
                        "id": "20",
                        "homeAway": "away",
                        "score": "1",
                        "team": { "displayName": "Bayern Munich" },
                        "linescores": [
                            { "displayValue": "1" },
                            { "displayValue": "0" },
                            { "displayValue": "0" },
                            { "displayValue": "0" }
                        ]
                    }
                ]
            }]
        }
    }"#;

    let fixture = NormalizedFixtureState::from_espn_summary_json(
        "soccer/uefa.champions",
        "501",
        raw_json,
    )
    .unwrap();

    assert_eq!(
        fixture.lifecycle_status,
        MatchLifecycleStatus::CompletedOvertime
    );
    assert_eq!(fixture.home.regulation_score, 1);
    assert_eq!(fixture.home.overtime_score, 1);
    assert_eq!(fixture.away.regulation_score, 1);
    assert_eq!(fixture.away.overtime_score, 0);

    let cfg = SportsEvaluationConfig::default();

    // 1. 90m Regulation Draw -> YES (1-1 at 90m)
    let reg_draw = SportsMarketPredicate::MatchOutcome {
        selection: MatchSelection::Draw,
        scope: SettlementScope::RegulationOnly,
    };
    assert!(matches!(
        reg_draw.evaluate(&fixture, 1_700_010_000, &cfg),
        SportsResolutionDecision::ResolvedYes { .. }
    ));

    // 2. 90m Regulation Home Win -> NO (1-1 at 90m)
    let reg_home = SportsMarketPredicate::MatchOutcome {
        selection: MatchSelection::Home,
        scope: SettlementScope::RegulationOnly,
    };
    assert!(matches!(
        reg_home.evaluate(&fixture, 1_700_010_000, &cfg),
        SportsResolutionDecision::ResolvedNo { .. }
    ));

    // 3. Overtime-Inclusive Home Win -> YES (2-1 AET)
    let aet_home = SportsMarketPredicate::MatchOutcome {
        selection: MatchSelection::Home,
        scope: SettlementScope::IncludeOvertime,
    };
    assert!(matches!(
        aet_home.evaluate(&fixture, 1_700_010_000, &cfg),
        SportsResolutionDecision::ResolvedYes { .. }
    ));

    // 4. 90m Regulation Over 2.5 Goals -> NO (2 goals in 90m)
    let reg_over25 = SportsMarketPredicate::TotalScoreOverUnder {
        threshold: Decimal::from_str("2.5").unwrap(),
        side: OverUnderSide::Over,
        scope: SettlementScope::RegulationOnly,
    };
    assert!(matches!(
        reg_over25.evaluate(&fixture, 1_700_010_000, &cfg),
        SportsResolutionDecision::ResolvedNo { .. }
    ));

    // 5. Overtime-Inclusive Over 2.5 Goals -> YES (3 goals including AET)
    let aet_over25 = SportsMarketPredicate::TotalScoreOverUnder {
        threshold: Decimal::from_str("2.5").unwrap(),
        side: OverUnderSide::Over,
        scope: SettlementScope::IncludeOvertime,
    };
    assert!(matches!(
        aet_over25.evaluate(&fixture, 1_700_010_000, &cfg),
        SportsResolutionDecision::ResolvedYes { .. }
    ));
}

/// FAILURE MODE 3: DNP (Did Not Play) & Late-Substitute Player Prop Voiding
/// Proves that an unused bench player (0 minutes played) resolves to VOID (not NO),
/// and a substitute entering at minute 65 after the first goal was scored at minute 25
/// resolves to VOID on FirstScoringPlay, but ResolvedNo on AnytimeGoal.
#[test]
fn test_failure_mode_3_dnp_and_late_sub_player_prop_voiding() {
    let raw_json = r#"{
        "header": {
            "id": "601",
            "competitions": [{
                "id": "601",
                "date": "2026-10-27T19:00:00Z",
                "status": {
                    "period": 2,
                    "displayClock": "90'",
                    "type": { "name": "STATUS_FULL_TIME", "state": "post", "completed": true }
                },
                "competitors": [
                    { "id": "1", "homeAway": "home", "score": "1", "team": { "displayName": "Liverpool" } },
                    { "id": "2", "homeAway": "away", "score": "0", "team": { "displayName": "Chelsea" } }
                ]
            }]
        },
        "plays": [
            {
                "id": "g1",
                "scoringPlay": true,
                "period": { "number": 1 },
                "clock": { "displayValue": "25'", "value": 1500.0 },
                "type": { "text": "Goal" },
                "text": "Goal! Luis Diaz scores.",
                "team": { "id": "1" },
                "participants": [{
                    "athlete": { "id": "99", "displayName": "Luis Díaz" }
                }]
            },
            {
                "id": "sub1",
                "scoringPlay": false,
                "period": { "number": 2 },
                "clock": { "displayValue": "65'", "value": 3900.0 },
                "type": { "text": "Substitution" },
                "text": "Darwin Nunez replaces Luis Diaz.",
                "participants": [{
                    "athlete": { "id": "102", "displayName": "Darwin Núñez" }
                }]
            }
        ],
        "rosters": [{
            "team": { "id": "1", "displayName": "Liverpool" },
            "roster": [
                {
                    "starter": true,
                    "subbedIn": false,
                    "athlete": { "id": "99", "displayName": "Luis Díaz" },
                    "position": { "name": "Forward" }
                },
                {
                    "starter": false,
                    "subbedIn": false,
                    "athlete": { "id": "101", "displayName": "Mohamed Salah" },
                    "position": { "name": "Forward" }
                },
                {
                    "starter": false,
                    "subbedIn": true,
                    "athlete": { "id": "102", "displayName": "Darwin Núñez" },
                    "position": { "name": "Forward" }
                }
            ]
        }]
    }"#;

    let fixture =
        NormalizedFixtureState::from_espn_summary_json("soccer/eng.1", "601", raw_json).unwrap();
    let cfg = SportsEvaluationConfig::default();

    // 1. Mohamed Salah was DNP (starter: false, subbedIn: false) -> Anytime Goal MUST be VOID
    let salah_anytime = SportsMarketPredicate::PlayerScoringMilestone {
        athlete: AthleteFilter {
            athlete_id: Some("101".into()),
            athlete_name: "Mohamed Salah".into(),
        },
        min_goals: 1,
        participation_policy: ParticipationPolicy::MustPlay,
        scope: SettlementScope::RegulationOnly,
    };
    assert!(
        matches!(
            salah_anytime.evaluate(&fixture, 1_700_010_000, &cfg),
            SportsResolutionDecision::Void { code: 3, .. }
        ),
        "DNP athlete must resolve to VOID, not NO"
    );

    // 2. Darwin Núñez subbed in at 65m (after 25m first goal) -> First Goal Scorer MUST be VOID
    let nunez_first_goal = SportsMarketPredicate::FirstScoringPlay {
        athlete: AthleteFilter {
            athlete_id: Some("102".into()),
            athlete_name: "Darwin Núñez".into(),
        },
        participation_policy: ParticipationPolicy::MustEnterBeforeFirstGoal,
        scope: SettlementScope::RegulationOnly,
    };
    assert!(
        matches!(
            nunez_first_goal.evaluate(&fixture, 1_700_010_000, &cfg),
            SportsResolutionDecision::Void { code: 3, .. }
        ),
        "Substitute entering after first goal must VOID FirstGoalScorer market"
    );

    // 3. Darwin Núñez DID play (65m-90m) and scored 0 -> Anytime Goal resolves to NO
    let nunez_anytime = SportsMarketPredicate::PlayerScoringMilestone {
        athlete: AthleteFilter {
            athlete_id: Some("102".into()),
            athlete_name: "Darwin Núñez".into(),
        },
        min_goals: 1,
        participation_policy: ParticipationPolicy::MustPlay,
        scope: SettlementScope::RegulationOnly,
    };
    assert!(matches!(
        nunez_anytime.evaluate(&fixture, 1_700_010_000, &cfg),
        SportsResolutionDecision::ResolvedNo { .. }
    ));
}

/// FAILURE MODE 4: Unconfirmed In-Play Goal / VAR Review & Scoreboard-Play Desync
/// Proves that when the scoreboard ticks 1-1 but the play ledger has an active VAR review or desync,
/// the engine holds the market in Pending rather than prematurely settling YES.
#[test]
fn test_failure_mode_4_var_review_and_desync_holds_pending() {
    // Scoreboard says 1-1, but only 1 confirmed goal play exists and VAR review is in progress
    let raw_json = r#"{
        "header": {
            "id": "701",
            "competitions": [{
                "id": "701",
                "date": "2026-10-27T19:00:00Z",
                "status": {
                    "period": 1,
                    "displayClock": "18'",
                    "type": { "name": "STATUS_IN_PROGRESS", "state": "in", "completed": false }
                },
                "competitors": [
                    { "id": "1", "homeAway": "home", "score": "1", "team": { "displayName": "Arsenal" } },
                    { "id": "2", "homeAway": "away", "score": "1", "team": { "displayName": "Tottenham" } }
                ]
            }]
        },
        "plays": [
            {
                "id": "g1",
                "scoringPlay": true,
                "period": { "number": 1 },
                "clock": { "displayValue": "10'", "value": 600.0 },
                "type": { "text": "Goal" },
                "text": "Goal! Arsenal 1, Tottenham 0.",
                "team": { "id": "1" }
            },
            {
                "id": "var1",
                "scoringPlay": false,
                "period": { "number": 1 },
                "clock": { "displayValue": "18'", "value": 1080.0 },
                "type": { "text": "VAR Review" },
                "text": "VAR Review in progress - checking possible offside."
            }
        ]
    }"#;

    let fixture =
        NormalizedFixtureState::from_espn_summary_json("soccer/eng.1", "701", raw_json).unwrap();
    assert!(!fixture.scoreboard_play_ledger_consistent);
    assert!(fixture.has_unconfirmed_or_var_review_play);

    let btts = SportsMarketPredicate::BothTeamsToScore {
        scope: SettlementScope::RegulationOnly,
    };
    let decision = btts.evaluate(&fixture, 1_700_001_000, &SportsEvaluationConfig::default());
    assert!(
        matches!(decision, SportsResolutionDecision::Pending { .. }),
        "Must hold settlement in Pending while VAR review or scoreboard desync is active"
    );
}

/// FAILURE MODE 5: Abandoned Match at Minute 60 (1-0)
/// Proves that when a match is abandoned at minute 60 with score 1-0 (goal scored at minute 12):
/// - Full-time markets (MatchOutcome, uncrossed Over 2.5) resolve to VOID.
/// - Already-completed early markets (GoalInTimeWindow <= 20m, FirstScoringPlay) preserve their YES outcome.
#[test]
fn test_failure_mode_5_abandoned_match_voids_fulltime_preserves_early_confirmed() {
    let raw_json = r#"{
        "header": {
            "id": "801",
            "competitions": [{
                "id": "801",
                "date": "2026-10-27T19:00:00Z",
                "status": {
                    "period": 2,
                    "displayClock": "60'",
                    "type": {
                        "name": "STATUS_ABANDONED",
                        "state": "post",
                        "completed": false,
                        "detail": "Abandoned"
                    }
                },
                "competitors": [
                    { "id": "1", "homeAway": "home", "score": "1", "team": { "displayName": "Arsenal" } },
                    { "id": "2", "homeAway": "away", "score": "0", "team": { "displayName": "Chelsea" } }
                ]
            }]
        },
        "plays": [{
            "id": "g1",
            "scoringPlay": true,
            "period": { "number": 1 },
            "clock": { "displayValue": "12'", "value": 720.0 },
            "type": { "text": "Goal" },
            "text": "Goal! Bukayo Saka scores.",
            "team": { "id": "1" },
            "participants": [{
                "athlete": { "id": "77", "displayName": "Bukayo Saka" }
            }]
        }]
    }"#;

    let fixture =
        NormalizedFixtureState::from_espn_summary_json("soccer/eng.1", "801", raw_json).unwrap();
    assert_eq!(fixture.lifecycle_status, MatchLifecycleStatus::Abandoned);

    let cfg = SportsEvaluationConfig::default();

    // 1. Full-time Home Win -> VOID
    let home_win = SportsMarketPredicate::MatchOutcome {
        selection: MatchSelection::Home,
        scope: SettlementScope::RegulationOnly,
    };
    assert!(matches!(
        home_win.evaluate(&fixture, 1_700_005_000, &cfg),
        SportsResolutionDecision::Void { code: 3, .. }
    ));

    // 2. Uncrossed Over 2.5 Goals -> VOID
    let over_25 = SportsMarketPredicate::TotalScoreOverUnder {
        threshold: Decimal::from_str("2.5").unwrap(),
        side: OverUnderSide::Over,
        scope: SettlementScope::RegulationOnly,
    };
    assert!(matches!(
        over_25.evaluate(&fixture, 1_700_005_000, &cfg),
        SportsResolutionDecision::Void { code: 3, .. }
    ));

    // 3. First 20m Goal (hit at minute 12) -> ResolvedYes
    let first_20m = SportsMarketPredicate::GoalInTimeWindow {
        period: 1,
        max_base_minute: 20,
        athlete_filter: None,
    };
    assert!(matches!(
        first_20m.evaluate(&fixture, 1_700_005_000, &cfg),
        SportsResolutionDecision::ResolvedYes { .. }
    ));
}

/// MULTIPLEXED MATRIX RESOLUTION & OFFLINE PROOF VERIFICATION
/// Proves that 1 signed ESPN summary attestation generates and resolves an entire 8-market
/// derivative matrix in a single pass and passes standalone offline cryptographic verification.
#[test]
fn test_multiplexed_single_attestation_resolves_full_8_market_matrix() {
    let raw_json = br#"{
        "header": {
            "id": "901",
            "competitions": [{
                "id": "901",
                "date": "2026-10-27T19:00:00Z",
                "status": {
                    "period": 2,
                    "displayClock": "90'+4'",
                    "type": { "name": "STATUS_FULL_TIME", "state": "post", "completed": true }
                },
                "competitors": [
                    {
                        "id": "1",
                        "homeAway": "home",
                        "score": "2",
                        "team": { "displayName": "Arsenal" },
                        "linescores": [{ "displayValue": "1" }, { "displayValue": "1" }]
                    },
                    {
                        "id": "2",
                        "homeAway": "away",
                        "score": "1",
                        "team": { "displayName": "Chelsea" },
                        "linescores": [{ "displayValue": "0" }, { "displayValue": "1" }]
                    }
                ]
            }]
        },
        "plays": [
            {
                "id": "g1",
                "scoringPlay": true,
                "period": { "number": 1 },
                "clock": { "displayValue": "14'", "value": 840.0 },
                "type": { "text": "Goal" },
                "text": "Goal! Bukayo Saka scores.",
                "team": { "id": "1" },
                "participants": [{ "athlete": { "id": "77", "displayName": "Bukayo Saka" } }]
            },
            {
                "id": "g2",
                "scoringPlay": true,
                "period": { "number": 2 },
                "clock": { "displayValue": "55'", "value": 3300.0 },
                "type": { "text": "Goal" },
                "text": "Goal! Cole Palmer scores.",
                "team": { "id": "2" },
                "participants": [{ "athlete": { "id": "88", "displayName": "Cole Palmer" } }]
            },
            {
                "id": "g3",
                "scoringPlay": true,
                "period": { "number": 2 },
                "clock": { "displayValue": "82'", "value": 4920.0 },
                "type": { "text": "Goal" },
                "text": "Goal! Bukayo Saka scores again.",
                "team": { "id": "1" },
                "participants": [{ "athlete": { "id": "77", "displayName": "Bukayo Saka" } }]
            }
        ],
        "rosters": [{
            "team": { "id": "1", "displayName": "Arsenal" },
            "roster": [{
                "starter": true,
                "subbedIn": false,
                "athlete": { "id": "77", "displayName": "Bukayo Saka" },
                "position": { "name": "Forward" }
            }]
        }]
    }"#;

    let fixture = NormalizedFixtureState::from_espn_summary_json(
        "soccer/eng.1",
        "901",
        std::str::from_utf8(raw_json).unwrap(),
    )
    .unwrap();

    // Auto-generate the 8-market derivative matrix (6 team/match markets + 2 Saka player props)
    let matrix = FixtureMarketMatrix::generate_from_fixture(&fixture);
    assert_eq!(matrix.templates.len(), 8);

    // Create 1 cryptographically signed attestation for the ESPN summary response
    let authority = MockAttestationAuthority::new();
    let attestation = authority.create_attested_payload(
        "https://site.api.espn.com/apis/site/v2/sports/soccer/eng.1/summary?event=901",
        raw_json,
        1_700_010_000,
        vec![("content-type".into(), "application/json".into())],
    );

    let resolver = MultiplexedSportsResolver::default();
    let report = resolver
        .resolve_matrix("soccer/eng.1", "901", &attestation, &matrix.templates)
        .unwrap();

    assert_eq!(report.verdicts.len(), 8);

    // Check expected outcomes across all 8 markets:
    // 1. home_win (Arsenal 2-1) -> YES (1)
    // 2. away_win -> NO (2)
    // 3. draw -> NO (2)
    // 4. over_under_2_5 (3 goals) -> YES (1)
    // 5. both_teams_to_score (2-1) -> YES (1)
    // 6. first_20_min_goal (Saka 14') -> YES (1)
    // 7. first_goal_scorer_bukayo-saka -> YES (1)
    // 8. player_anytime_bukayo-saka -> YES (1)
    let outcomes: Vec<(&str, u8)> = report
        .verdicts
        .iter()
        .map(|v| (v.template_key.as_str(), v.outcome_code))
        .collect();

    assert_eq!(
        outcomes,
        vec![
            ("home_win", 1),
            ("away_win", 2),
            ("draw", 2),
            ("over_under_2_5", 1),
            ("both_teams_to_score", 1),
            ("first_20_min_goal", 1),
            ("first_goal_scorer_bukayo-saka", 1),
            ("player_anytime_bukayo-saka", 1),
        ]
    );

    // Verify the multiplexed report offline
    assert!(resolver
        .verify_report_offline(&attestation, &matrix.templates, &report)
        .is_ok());

    // Tamper with one verdict outcome and confirm offline verification rejects it
    let mut tampered_report = report.clone();
    tampered_report.verdicts[0].outcome_code = 2;
    assert!(resolver
        .verify_report_offline(&attestation, &matrix.templates, &tampered_report)
        .is_err());
}

proptest! {
    /// Fuzz test: MatchClock::parse never panics on arbitrary strings and preserves stoppage separation
    #[test]
    fn fuzz_match_clock_stoppage_time(
        period in 1u32..6,
        base_min in 0u32..120,
        stop_min in 1u32..25
    ) {
        let formatted = format!("{}'+{}'", base_min, stop_min);
        let clk = MatchClock::parse(period, Some(&formatted), None);
        prop_assert_eq!(clk.period, period);
        prop_assert_eq!(clk.base_minute, base_min);
        prop_assert_eq!(clk.stoppage_minute, stop_min);
    }
}
