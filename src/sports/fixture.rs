use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use thiserror::Error;

#[derive(Error, Debug, PartialEq, Eq, Clone)]
pub enum FixtureParseError {
    #[error("Invalid ESPN summary JSON: {0}")]
    InvalidJson(String),
    #[error("Missing competition header in fixture summary")]
    MissingHeader,
    #[error("Missing competitors in fixture summary")]
    MissingCompetitors,
    #[error("Entity validation failed: fixture teams '{home} vs {away}' do not match expected market context '{expected}'")]
    EntityMismatch {
        home: String,
        away: String,
        expected: String,
    },
    #[error("Scoreboard inconsistency detected: {0}")]
    ScoreboardDesync(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HomeAway {
    Home,
    Away,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MatchLifecycleStatus {
    PreMatch,
    InPlay,
    CompletedRegulation,
    CompletedOvertime,
    CompletedShootout,
    Postponed,
    Abandoned,
    Canceled,
}

impl MatchLifecycleStatus {
    pub fn is_terminal_completed(&self) -> bool {
        matches!(
            self,
            Self::CompletedRegulation | Self::CompletedOvertime | Self::CompletedShootout
        )
    }

    pub fn is_terminal_aborted(&self) -> bool {
        matches!(self, Self::Postponed | Self::Abandoned | Self::Canceled)
    }
}

/// Stoppage-time-aware game clock that prevents the "45'+4' -> 454" concatenation bug
/// and maintains strict chronological ordering across periods.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatchClock {
    pub period: u32,
    pub base_minute: u32,
    pub stoppage_minute: u32,
    pub elapsed_seconds_in_period: u32,
}

impl MatchClock {
    /// Parses ESPN displayClock strings such as `"45'+4'"`, `"90'+2'"`, `"19:45"`, `"23'"`,
    /// combined with period number and optional raw clock value in seconds.
    pub fn parse(period: u32, display_clock: Option<&str>, raw_seconds: Option<f64>) -> Self {
        let effective_period = period.max(1);

        if let Some(clk_str) = display_clock {
            let trimmed = clk_str.trim();
            if !trimmed.is_empty() {
                // Case 1: Stoppage time format e.g. "45'+4'" or "90+3"
                if let Some((base_part, stop_part)) = trimmed.split_once('+') {
                    let base_digits: String =
                        base_part.chars().filter(|c| c.is_ascii_digit()).collect();
                    let stop_digits: String =
                        stop_part.chars().filter(|c| c.is_ascii_digit()).collect();

                    let base_min = base_digits.parse::<u32>().unwrap_or(0);
                    let stop_min = stop_digits.parse::<u32>().unwrap_or(0);
                    let total_secs = (base_min + stop_min) * 60;

                    return Self {
                        period: effective_period,
                        base_minute: base_min,
                        stoppage_minute: stop_min,
                        elapsed_seconds_in_period: total_secs,
                    };
                }

                // Case 2: MM:SS format e.g. "19:45" or "04:12"
                if let Some((min_part, sec_part)) = trimmed.split_once(':') {
                    let min_digits: String =
                        min_part.chars().filter(|c| c.is_ascii_digit()).collect();
                    let sec_digits: String =
                        sec_part.chars().filter(|c| c.is_ascii_digit()).collect();

                    let mins = min_digits.parse::<u32>().unwrap_or(0);
                    let secs = sec_digits.parse::<u32>().unwrap_or(0);
                    let total_secs = mins * 60 + secs;

                    return Self {
                        period: effective_period,
                        base_minute: mins,
                        stoppage_minute: 0,
                        elapsed_seconds_in_period: total_secs,
                    };
                }

                // Case 3: Single minute marker e.g. "23'" or "45"
                let digits: String = trimmed.chars().filter(|c| c.is_ascii_digit()).collect();
                if let Ok(mins) = digits.parse::<u32>() {
                    let secs = raw_seconds
                        .map(|s| s.round() as u32)
                        .unwrap_or(mins * 60);
                    return Self {
                        period: effective_period,
                        base_minute: mins,
                        stoppage_minute: 0,
                        elapsed_seconds_in_period: secs,
                    };
                }
            }
        }

        // Case 4: Fallback to raw_seconds
        if let Some(secs_f64) = raw_seconds {
            let total_secs = secs_f64.max(0.0).round() as u32;
            let total_mins = total_secs / 60;

            let (base_minute, stoppage_minute) = if effective_period == 1 && total_mins > 45 {
                (45, total_mins - 45)
            } else if effective_period == 2 && total_mins > 90 {
                (90, total_mins - 90)
            } else {
                (total_mins, 0)
            };

            return Self {
                period: effective_period,
                base_minute,
                stoppage_minute,
                elapsed_seconds_in_period: total_secs,
            };
        }

        Self {
            period: effective_period,
            base_minute: 0,
            stoppage_minute: 0,
            elapsed_seconds_in_period: 0,
        }
    }
}

impl PartialOrd for MatchClock {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for MatchClock {
    fn cmp(&self, other: &Self) -> Ordering {
        (
            self.period,
            self.base_minute,
            self.stoppage_minute,
            self.elapsed_seconds_in_period,
        )
            .cmp(&(
                other.period,
                other.base_minute,
                other.stoppage_minute,
                other.elapsed_seconds_in_period,
            ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamScoreBreakdown {
    pub team_id: String,
    pub team_name: String,
    pub home_away: HomeAway,
    pub reported_total_score: u32,
    pub regulation_score: u32,
    pub overtime_score: u32,
    pub shootout_score: Option<u32>,
    pub period_scores: Vec<u32>,
    pub has_explicit_period_breakdown: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfirmedScoringPlay {
    pub play_id: String,
    pub clock: MatchClock,
    pub team_id: Option<String>,
    pub primary_scorer_id: Option<String>,
    pub primary_scorer_name: Option<String>,
    pub normalized_scorer_name: Option<String>,
    pub is_own_goal: bool,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ParticipationStatus {
    Started,
    SubbedIn { minute: Option<u32> },
    DidNotPlay,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AthleteParticipation {
    pub athlete_id: String,
    pub display_name: String,
    pub normalized_name: String,
    pub team_id: String,
    pub team_name: String,
    pub position: Option<String>,
    pub status: ParticipationStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NormalizedFixtureState {
    pub event_id: String,
    pub sport_path: String,
    pub regulation_periods: u32,
    pub kickoff_timestamp: Option<i64>,
    pub lifecycle_status: MatchLifecycleStatus,
    pub current_clock: MatchClock,
    pub home: TeamScoreBreakdown,
    pub away: TeamScoreBreakdown,
    pub confirmed_scoring_plays: Vec<ConfirmedScoringPlay>,
    pub has_unconfirmed_or_var_review_play: bool,
    pub scoreboard_play_ledger_consistent: bool,
    pub athletes: Vec<AthleteParticipation>,
}

/// Normalizes names by lowercasing and stripping common Latin diacritics
pub fn normalize_athlete_name(s: &str) -> String {
    s.trim()
        .to_lowercase()
        .chars()
        .map(|c| match c {
            'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' => 'a',
            'è' | 'é' | 'ê' | 'ë' => 'e',
            'ì' | 'í' | 'î' | 'ï' => 'i',
            'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' => 'o',
            'ù' | 'ú' | 'û' | 'ü' => 'u',
            'ñ' => 'n',
            'ç' | 'ć' | 'č' => 'c',
            'ý' | 'ÿ' => 'y',
            'š' | 'ś' => 's',
            'ž' | 'ź' | 'ż' => 'z',
            other => other,
        })
        .collect()
}

impl NormalizedFixtureState {
    pub fn from_espn_summary_json(
        sport_path: &str,
        fallback_event_id: &str,
        raw_json: &str,
    ) -> Result<Self, FixtureParseError> {
        let root: JsonValue = serde_json::from_str(raw_json)
            .map_err(|e| FixtureParseError::InvalidJson(e.to_string()))?;

        let header = root
            .get("header")
            .ok_or(FixtureParseError::MissingHeader)?;
        let comps = header
            .get("competitions")
            .and_then(|c| c.as_array())
            .ok_or(FixtureParseError::MissingHeader)?;
        let comp = comps.first().ok_or(FixtureParseError::MissingHeader)?;

        let event_id = comp
            .get("id")
            .or_else(|| header.get("id"))
            .and_then(|v| v.as_str())
            .unwrap_or(fallback_event_id)
            .to_string();

        let kickoff_timestamp = comp
            .get("date")
            .and_then(|d| d.as_str())
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.timestamp());

        let regulation_periods = if sport_path.starts_with("soccer") {
            2
        } else if sport_path.starts_with("basketball") || sport_path.starts_with("football") {
            4
        } else if sport_path.starts_with("hockey") {
            3
        } else {
            2
        };

        // Parse status & clock
        let status_obj = comp.get("status");
        let status_type = status_obj.and_then(|s| s.get("type"));

        let state_str = status_type
            .and_then(|t| t.get("state"))
            .and_then(|v| v.as_str())
            .unwrap_or("pre");
        let type_name = status_type
            .and_then(|t| t.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_uppercase();
        let detail_str = status_type
            .and_then(|t| t.get("detail").or_else(|| t.get("shortDetail")))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_uppercase();
        let is_completed = status_type
            .and_then(|t| t.get("completed"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let period = status_obj
            .and_then(|s| s.get("period"))
            .and_then(|p| p.as_u64())
            .unwrap_or(1) as u32;
        let display_clock = status_obj
            .and_then(|s| s.get("displayClock"))
            .and_then(|c| c.as_str());
        let raw_clock_val = status_obj
            .and_then(|s| s.get("clock"))
            .and_then(|c| c.as_f64());

        let current_clock = MatchClock::parse(period, display_clock, raw_clock_val);

        // Parse competitors & linescores
        let competitors = comp
            .get("competitors")
            .and_then(|c| c.as_array())
            .ok_or(FixtureParseError::MissingCompetitors)?;

        let mut home_breakdown: Option<TeamScoreBreakdown> = None;
        let mut away_breakdown: Option<TeamScoreBreakdown> = None;
        let mut any_shootout = false;
        let mut max_linescore_periods = 0usize;

        for c in competitors {
            let team_id = c
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let team_name = c
                .get("team")
                .and_then(|t| t.get("displayName").or_else(|| t.get("name")))
                .and_then(|v| v.as_str())
                .unwrap_or("Unknown")
                .to_string();
            let home_away = match c.get("homeAway").and_then(|v| v.as_str()) {
                Some("away") => HomeAway::Away,
                _ => HomeAway::Home,
            };
            let reported_total_score = c
                .get("score")
                .and_then(|v| {
                    v.as_str()
                        .and_then(|s| s.parse::<u32>().ok())
                        .or_else(|| v.as_u64().map(|n| n as u32))
                })
                .unwrap_or(0);

            let shootout_score = c.get("shootoutScore").and_then(|v| {
                v.as_str()
                    .and_then(|s| s.parse::<u32>().ok())
                    .or_else(|| v.as_u64().map(|n| n as u32))
            });
            if shootout_score.is_some() {
                any_shootout = true;
            }

            let mut period_scores = Vec::new();
            if let Some(ls_arr) = c.get("linescores").and_then(|l| l.as_array()) {
                for ls in ls_arr {
                    let val = ls
                        .get("displayValue")
                        .and_then(|v| v.as_str())
                        .and_then(|s| s.parse::<u32>().ok())
                        .or_else(|| ls.get("value").and_then(|v| v.as_f64()).map(|f| f as u32))
                        .unwrap_or(0);
                    period_scores.push(val);
                }
            }

            max_linescore_periods = max_linescore_periods.max(period_scores.len());

            let has_explicit_period_breakdown = !period_scores.is_empty();
            let (regulation_score, overtime_score) = if has_explicit_period_breakdown {
                let reg_len = (regulation_periods as usize).min(period_scores.len());
                let reg_sum: u32 = period_scores[..reg_len].iter().sum();
                let ot_sum: u32 = period_scores[reg_len..].iter().sum();
                (reg_sum, ot_sum)
            } else {
                (reported_total_score, 0)
            };

            let breakdown = TeamScoreBreakdown {
                team_id,
                team_name,
                home_away,
                reported_total_score,
                regulation_score,
                overtime_score,
                shootout_score,
                period_scores,
                has_explicit_period_breakdown,
            };

            match home_away {
                HomeAway::Home => home_breakdown = Some(breakdown),
                HomeAway::Away => away_breakdown = Some(breakdown),
            }
        }

        let mut home = home_breakdown.ok_or(FixtureParseError::MissingCompetitors)?;
        let mut away = away_breakdown.ok_or(FixtureParseError::MissingCompetitors)?;

        // Determine exact lifecycle status
        let lifecycle_status = if type_name.contains("ABANDON")
            || detail_str.contains("ABANDON")
            || type_name.contains("SUSPEND")
        {
            MatchLifecycleStatus::Abandoned
        } else if type_name.contains("POSTPON") || detail_str.contains("POSTPON") {
            MatchLifecycleStatus::Postponed
        } else if type_name.contains("CANCEL") || detail_str.contains("CANCEL") {
            MatchLifecycleStatus::Canceled
        } else if !is_completed && state_str.eq_ignore_ascii_case("pre") {
            MatchLifecycleStatus::PreMatch
        } else if !is_completed && state_str.eq_ignore_ascii_case("in") {
            MatchLifecycleStatus::InPlay
        } else if is_completed || state_str.eq_ignore_ascii_case("post") {
            if any_shootout
                || type_name.contains("PEN")
                || detail_str.contains("PEN")
                || detail_str.contains("SHOOTOUT")
            {
                MatchLifecycleStatus::CompletedShootout
            } else if type_name.contains("AET")
                || type_name.contains("OVERTIME")
                || detail_str.contains("AET")
                || detail_str.contains("ET")
                || detail_str.contains("OT")
                || max_linescore_periods > regulation_periods as usize
                || period > regulation_periods
            {
                MatchLifecycleStatus::CompletedOvertime
            } else {
                MatchLifecycleStatus::CompletedRegulation
            }
        } else {
            MatchLifecycleStatus::InPlay
        };

        // Parse plays & keyEvents
        let mut raw_plays = Vec::new();
        if let Some(p_arr) = root.get("plays").and_then(|v| v.as_array()) {
            raw_plays.extend(p_arr.iter());
        }
        if let Some(ke_arr) = root.get("keyEvents").and_then(|v| v.as_array()) {
            raw_plays.extend(ke_arr.iter());
        }

        let mut seen_play_keys = HashSet::new();
        let mut confirmed_scoring_plays = Vec::new();
        let mut has_unconfirmed_or_var_review_play = false;
        let mut substituted_in_athletes: HashMap<String, Option<u32>> = HashMap::new();

        for (idx, play) in raw_plays.iter().enumerate() {
            let play_id = play
                .get("id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| format!("play_{}", idx));

            let ptype_text = play
                .get("type")
                .and_then(|t| t.get("text"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let ptype_lower = ptype_text.to_lowercase();

            let text = play
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let short_text = play
                .get("shortText")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let text_lower = text.to_lowercase();
            let short_lower = short_text.to_lowercase();

            let play_period = play
                .get("period")
                .and_then(|p| p.get("number"))
                .and_then(|n| n.as_u64())
                .unwrap_or(1) as u32;
            let play_disp_clock = play
                .get("clock")
                .and_then(|c| c.get("displayValue"))
                .and_then(|v| v.as_str());
            let play_raw_clock = play
                .get("clock")
                .and_then(|c| c.get("value"))
                .and_then(|v| v.as_f64());

            let play_clock = MatchClock::parse(play_period, play_disp_clock, play_raw_clock);

            // Detect substitution events for participation tracking
            if ptype_lower.contains("substitution") || text_lower.contains("substitution") {
                if let Some(parts) = play.get("participants").and_then(|p| p.as_array()) {
                    if let Some(first_p) = parts.first() {
                        if let Some(aid) = first_p
                            .get("athlete")
                            .and_then(|a| a.get("id"))
                            .and_then(|v| v.as_str())
                        {
                            substituted_in_athletes
                                .insert(aid.to_string(), Some(play_clock.base_minute));
                        }
                    }
                }
            }

            // Detect active VAR review or overturned play
            if text_lower.contains("var review")
                || text_lower.contains("under review")
                || ptype_lower.contains("var")
            {
                has_unconfirmed_or_var_review_play = true;
            }

            let is_disallowed = ptype_lower.contains("disallowed")
                || text_lower.contains("disallowed")
                || short_lower.contains("disallowed")
                || text_lower.contains("overturned")
                || ptype_lower.contains("miss")
                || text_lower.contains("missed")
                || ptype_lower.contains("saved")
                || ptype_lower.contains("post");

            let is_scoring_flag = play
                .get("scoringPlay")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let mentions_goal = ptype_lower.contains("goal")
                || ptype_lower.contains("penalty - scored")
                || text_lower.starts_with("goal!");

            if (is_scoring_flag || mentions_goal) && !is_disallowed {
                // Exclude shootout penalties (period > 4 in soccer) from regular play ledger
                if sport_path.starts_with("soccer") && play_period > 4 {
                    continue;
                }

                let dedup_key = format!(
                    "{}:{}:{}:{}",
                    play_clock.period,
                    play_clock.base_minute,
                    play_clock.stoppage_minute,
                    play_id
                );
                if !seen_play_keys.insert(dedup_key) {
                    continue;
                }

                let is_own_goal =
                    ptype_lower.contains("own goal") || text_lower.contains("own goal");

                let team_id = play
                    .get("team")
                    .and_then(|t| t.get("id"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());

                let mut primary_scorer_id = None;
                let mut primary_scorer_name = None;

                if let Some(parts) = play.get("participants").and_then(|p| p.as_array()) {
                    if let Some(first) = parts.first() {
                        if let Some(ath) = first.get("athlete") {
                            primary_scorer_id = ath
                                .get("id")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string());
                            primary_scorer_name = ath
                                .get("displayName")
                                .or_else(|| ath.get("shortName"))
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string());
                        }
                    }
                }

                let normalized_scorer_name =
                    primary_scorer_name.as_deref().map(normalize_athlete_name);

                confirmed_scoring_plays.push(ConfirmedScoringPlay {
                    play_id,
                    clock: play_clock,
                    team_id,
                    primary_scorer_id,
                    primary_scorer_name,
                    normalized_scorer_name,
                    is_own_goal,
                    description: if !text.is_empty() {
                        text.to_string()
                    } else {
                        short_text.to_string()
                    },
                });
            }
        }

        confirmed_scoring_plays.sort_by(|a, b| a.clock.cmp(&b.clock));

        // If fixture went to overtime/shootout without explicit linescores, derive regulation vs OT from play periods
        if matches!(
            lifecycle_status,
            MatchLifecycleStatus::CompletedOvertime | MatchLifecycleStatus::CompletedShootout
        ) && (!home.has_explicit_period_breakdown || !away.has_explicit_period_breakdown)
            && !confirmed_scoring_plays.is_empty()
        {
            let mut h_reg = 0u32;
            let mut h_ot = 0u32;
            let mut a_reg = 0u32;
            let mut a_ot = 0u32;
            let mut all_tagged = true;

            for p in &confirmed_scoring_plays {
                if let Some(tid) = &p.team_id {
                    if tid == &home.team_id {
                        if p.clock.period <= regulation_periods {
                            h_reg += 1;
                        } else {
                            h_ot += 1;
                        }
                    } else if tid == &away.team_id {
                        if p.clock.period <= regulation_periods {
                            a_reg += 1;
                        } else {
                            a_ot += 1;
                        }
                    } else {
                        all_tagged = false;
                    }
                } else {
                    all_tagged = false;
                }
            }

            if all_tagged
                && (h_reg + h_ot == home.reported_total_score)
                && (a_reg + a_ot == away.reported_total_score)
            {
                home.regulation_score = h_reg;
                home.overtime_score = h_ot;
                home.has_explicit_period_breakdown = true;
                away.regulation_score = a_reg;
                away.overtime_score = a_ot;
                away.has_explicit_period_breakdown = true;
            }
        }

        // Check internal consistency between scoreboard and linescores / play ledger
        let mut scoreboard_play_ledger_consistent = true;
        if home.has_explicit_period_breakdown
            && (home.regulation_score + home.overtime_score != home.reported_total_score)
        {
            scoreboard_play_ledger_consistent = false;
        }
        if away.has_explicit_period_breakdown
            && (away.regulation_score + away.overtime_score != away.reported_total_score)
        {
            scoreboard_play_ledger_consistent = false;
        }

        // In soccer, if plays/keyEvents are provided, verify total goals on scoreboard matches confirmed goal plays
        if sport_path.starts_with("soccer") && !raw_plays.is_empty() {
            let total_reported = home.reported_total_score + away.reported_total_score;
            if confirmed_scoring_plays.len() as u32 != total_reported {
                scoreboard_play_ledger_consistent = false;
            }
        }

        // Parse rosters for athlete participation tracking
        let mut athletes = Vec::new();
        if let Some(rosters) = root.get("rosters").and_then(|r| r.as_array()) {
            for r in rosters {
                let team_obj = r.get("team");
                let team_id = team_obj
                    .and_then(|t| t.get("id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let team_name = team_obj
                    .and_then(|t| t.get("displayName").or_else(|| t.get("name")))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();

                if let Some(entries) = r.get("roster").and_then(|e| e.as_array()) {
                    for entry in entries {
                        if let Some(ath) = entry.get("athlete") {
                            let aid = ath
                                .get("id")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let disp_name = ath
                                .get("displayName")
                                .or_else(|| ath.get("shortName"))
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();

                            if aid.is_empty() && disp_name.is_empty() {
                                continue;
                            }

                            let position = entry
                                .get("position")
                                .and_then(|p| p.get("name").or_else(|| p.get("displayName")))
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string());

                            let is_starter = entry
                                .get("starter")
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false);
                            let is_subbed_in = entry
                                .get("subbedIn")
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false)
                                || substituted_in_athletes.contains_key(&aid);

                            let status = if is_starter {
                                ParticipationStatus::Started
                            } else if is_subbed_in {
                                let minute =
                                    substituted_in_athletes.get(&aid).copied().flatten();
                                ParticipationStatus::SubbedIn { minute }
                            } else {
                                ParticipationStatus::DidNotPlay
                            };

                            athletes.push(AthleteParticipation {
                                athlete_id: aid,
                                normalized_name: normalize_athlete_name(&disp_name),
                                display_name: disp_name,
                                team_id: team_id.clone(),
                                team_name: team_name.clone(),
                                position,
                                status,
                            });
                        }
                    }
                }
            }
        }

        Ok(Self {
            event_id,
            sport_path: sport_path.to_string(),
            regulation_periods,
            kickoff_timestamp,
            lifecycle_status,
            current_clock,
            home,
            away,
            confirmed_scoring_plays,
            has_unconfirmed_or_var_review_play,
            scoreboard_play_ledger_consistent,
            athletes,
        })
    }

    /// Validates that the fixture teams correspond to the expected market question/slug
    pub fn validate_entities(&self, market_context_text: &str) -> Result<(), FixtureParseError> {
        let ctx_norm = normalize_athlete_name(market_context_text);
        let home_norm = normalize_athlete_name(&self.home.team_name);
        let away_norm = normalize_athlete_name(&self.away.team_name);

        let matches_token = |team_norm: &str| {
            team_norm
                .split_whitespace()
                .any(|part| part.len() >= 3 && ctx_norm.contains(part))
        };

        if matches_token(&home_norm) || matches_token(&away_norm) {
            Ok(())
        } else {
            Err(FixtureParseError::EntityMismatch {
                home: self.home.team_name.clone(),
                away: self.away.team_name.clone(),
                expected: market_context_text.to_string(),
            })
        }
    }

    /// Look up athlete participation by ID or normalized name
    pub fn find_athlete(
        &self,
        athlete_id: Option<&str>,
        athlete_name: Option<&str>,
    ) -> Option<&AthleteParticipation> {
        if let Some(id) = athlete_id {
            if !id.is_empty() {
                if let Some(found) = self.athletes.iter().find(|a| a.athlete_id == id) {
                    return Some(found);
                }
            }
        }
        if let Some(name) = athlete_name {
            let norm = normalize_athlete_name(name);
            if !norm.is_empty() {
                if let Some(found) = self
                    .athletes
                    .iter()
                    .find(|a| a.normalized_name.contains(&norm) || norm.contains(&a.normalized_name))
                {
                    return Some(found);
                }
            }
        }
        None
    }
}
