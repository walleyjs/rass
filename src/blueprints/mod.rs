use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::ast::{ComparisonOperator, Expression, Value};
use crate::fsm::TemporalRules;

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(tag = "archetype", content = "params")]
pub enum MarketBlueprint {
    CryptoSpotCross {
        symbol: String,
        strike_price: Decimal,
        operator: ComparisonOperator,
        cutoff_time: DateTime<Utc>,
    },
    DeFiMetric {
        protocol: String,
        metric: String,
        threshold: Decimal,
        operator: ComparisonOperator,
        cutoff_time: DateTime<Utc>,
    },
    EconomicIndicator {
        indicator_code: String,
        threshold: Decimal,
        operator: ComparisonOperator,
        cutoff_time: DateTime<Utc>,
    },
    WeatherThreshold {
        location: String,
        latitude: Decimal,
        longitude: Decimal,
        threshold_celsius: Decimal,
        operator: ComparisonOperator,
        cutoff_time: DateTime<Utc>,
    },
    SoftwareRelease {
        owner: String,
        repo: String,
        target_version: String,
        operator: ComparisonOperator,
        cutoff_time: DateTime<Utc>,
    },
    SportsMatch {
        event_id: String,
        target_team: String,
        condition: String,
        kickoff_time: DateTime<Utc>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompiledMarketSpec {
    pub question: String,
    pub primary_source_url: String,
    pub secondary_source_urls: Vec<String>,
    pub ast_expression: Expression,
    pub temporal_rules: TemporalRules,
    pub canonical_rulebook: String,
}

impl MarketBlueprint {
    pub fn compile(&self) -> CompiledMarketSpec {
        match self {
            Self::CryptoSpotCross {
                symbol,
                strike_price,
                operator,
                cutoff_time,
            } => {
                let sym_upper = symbol.to_uppercase();
                let pair = if sym_upper.ends_with("USDT") {
                    sym_upper.clone()
                } else {
                    format!("{}USDT", sym_upper)
                };
                let base = pair.trim_end_matches("USDT");
                let primary_url = format!(
                    "https://api.binance.com/api/v3/ticker/price?symbol={}",
                    pair
                );
                let secondary_url = format!(
                    "https://data-api.binance.vision/api/v3/ticker/price?symbol={}",
                    pair
                );

                let op_phrase = match operator {
                    ComparisonOperator::GreaterThan => "exceed",
                    ComparisonOperator::GreaterThanOrEqual => "reach or exceed",
                    ComparisonOperator::LessThan => "drop below",
                    ComparisonOperator::LessThanOrEqual => "stay at or below",
                    _ => "equal",
                };

                let question = format!(
                    "Will {} ({}) {} ${} at {} UTC?",
                    base,
                    pair,
                    op_phrase,
                    strike_price,
                    cutoff_time.format("%Y-%m-%d %H:%M")
                );

                let rulebook = format!(
                    "RULEBOOK SPECIFICATION: Market resolves YES if the official Binance Spot API endpoint {} reports a price that is {:?} to {} as of cutoff timestamp {}. Secondary mirror {} serves as failover witness.",
                    primary_url, operator, strike_price, cutoff_time.timestamp(), secondary_url
                );

                // AST: ParseDecimal(JsonPath("$.price", PayloadRoot)) `operator` Literal(Decimal(strike_price))
                let ast = Expression::Compare {
                    op: *operator,
                    left: Box::new(Expression::ParseDecimal {
                        target: Box::new(Expression::JsonPath {
                            path: "price".to_string(),
                            target: Box::new(Expression::PayloadRoot),
                        }),
                    }),
                    right: Box::new(Expression::Literal {
                        value: Value::Decimal(*strike_price),
                    }),
                };

                let temporal = TemporalRules {
                    cutoff_time: cutoff_time.timestamp(),
                    challenge_window_seconds: 1800,  // 30 minutes
                    retroactive_freeze_seconds: 600, // 10 minutes
                };

                CompiledMarketSpec {
                    question,
                    primary_source_url: primary_url,
                    secondary_source_urls: vec![secondary_url],
                    ast_expression: ast,
                    temporal_rules: temporal,
                    canonical_rulebook: rulebook,
                }
            }

            Self::DeFiMetric {
                protocol,
                metric,
                threshold,
                operator,
                cutoff_time,
            } => {
                let primary_url = format!(
                    "https://api.llama.fi/{}/{}",
                    metric.to_lowercase(),
                    protocol.to_lowercase()
                );

                let question = format!(
                    "Will {} {} {:?} {} at {} UTC?",
                    protocol,
                    metric.to_uppercase(),
                    operator,
                    threshold,
                    cutoff_time.format("%Y-%m-%d %H:%M")
                );

                let ast = Expression::Compare {
                    op: *operator,
                    left: Box::new(Expression::ParseDecimal {
                        target: Box::new(Expression::RegexMatch {
                            pattern: r"([0-9]+\.?[0-9]*)".to_string(),
                            capture_group: 1,
                            target: Box::new(Expression::PayloadRoot),
                        }),
                    }),
                    right: Box::new(Expression::Literal {
                        value: Value::Decimal(*threshold),
                    }),
                };

                CompiledMarketSpec {
                    question: question.clone(),
                    primary_source_url: primary_url,
                    secondary_source_urls: vec![],
                    ast_expression: ast,
                    temporal_rules: TemporalRules {
                        cutoff_time: cutoff_time.timestamp(),
                        challenge_window_seconds: 3600,
                        retroactive_freeze_seconds: 1800,
                    },
                    canonical_rulebook: format!(
                        "DeFi Metric market for {} querying DefiLlama API.",
                        protocol
                    ),
                }
            }

            Self::EconomicIndicator {
                indicator_code,
                threshold,
                operator,
                cutoff_time,
            } => {
                let primary_url = format!(
                    "https://api.stlouisfed.org/fred/series/observations?series_id={}&limit=1",
                    indicator_code
                );

                let question = format!(
                    "Will {} {:?} {} upon release?",
                    indicator_code, operator, threshold
                );

                let ast = Expression::Compare {
                    op: *operator,
                    left: Box::new(Expression::ParseDecimal {
                        target: Box::new(Expression::JsonPath {
                            path: "observations[0].value".to_string(),
                            target: Box::new(Expression::PayloadRoot),
                        }),
                    }),
                    right: Box::new(Expression::Literal {
                        value: Value::Decimal(*threshold),
                    }),
                };

                CompiledMarketSpec {
                    question,
                    primary_source_url: primary_url,
                    secondary_source_urls: vec![],
                    ast_expression: ast,
                    temporal_rules: TemporalRules {
                        cutoff_time: cutoff_time.timestamp(),
                        challenge_window_seconds: 7200,
                        retroactive_freeze_seconds: 3600,
                    },
                    canonical_rulebook: format!(
                        "Economic indicator {} release from FRED. Strict freeze period 3600s prohibits retroactive revisions.",
                        indicator_code
                    ),
                }
            }

            Self::WeatherThreshold {
                location,
                latitude,
                longitude,
                threshold_celsius,
                operator,
                cutoff_time,
            } => {
                let primary_url = format!(
                    "https://api.open-meteo.com/v1/forecast?latitude={}&longitude={}&current=temperature_2m",
                    latitude, longitude
                );

                let question = format!(
                    "Will temperature in {} {:?} {}°C at {} UTC?",
                    location,
                    operator,
                    threshold_celsius,
                    cutoff_time.format("%Y-%m-%d %H:%M")
                );

                let ast = Expression::Compare {
                    op: *operator,
                    left: Box::new(Expression::ParseDecimal {
                        target: Box::new(Expression::JsonPath {
                            path: "current.temperature_2m".to_string(),
                            target: Box::new(Expression::PayloadRoot),
                        }),
                    }),
                    right: Box::new(Expression::Literal {
                        value: Value::Decimal(*threshold_celsius),
                    }),
                };

                CompiledMarketSpec {
                    question,
                    primary_source_url: primary_url,
                    secondary_source_urls: vec![],
                    ast_expression: ast,
                    temporal_rules: TemporalRules {
                        cutoff_time: cutoff_time.timestamp(),
                        challenge_window_seconds: 3600,
                        retroactive_freeze_seconds: 1800,
                    },
                    canonical_rulebook: format!(
                        "Open-Meteo temperature oracle for {} at lat {}, lon {}",
                        location, latitude, longitude
                    ),
                }
            }

            Self::SoftwareRelease {
                owner,
                repo,
                target_version,
                operator,
                cutoff_time,
            } => {
                let primary_url = format!(
                    "https://api.github.com/repos/{}/{}/releases/latest",
                    owner, repo
                );

                let question = format!(
                    "Will {}/{} release a version {:?} {} before {} UTC?",
                    owner,
                    repo,
                    operator,
                    target_version,
                    cutoff_time.format("%Y-%m-%d %H:%M")
                );

                let target_sv = crate::ast::ParsedSemVer::parse(target_version).unwrap_or(
                    crate::ast::ParsedSemVer {
                        major: 0,
                        minor: 0,
                        patch: 0,
                        pre_release: None,
                    },
                );

                let ast = Expression::Compare {
                    op: *operator,
                    left: Box::new(Expression::ParseSemVer {
                        target: Box::new(Expression::JsonPath {
                            path: "tag_name".to_string(),
                            target: Box::new(Expression::PayloadRoot),
                        }),
                    }),
                    right: Box::new(Expression::Literal {
                        value: Value::SemVer(target_sv),
                    }),
                };

                CompiledMarketSpec {
                    question,
                    primary_source_url: primary_url,
                    secondary_source_urls: vec![],
                    ast_expression: ast,
                    temporal_rules: TemporalRules {
                        cutoff_time: cutoff_time.timestamp(),
                        challenge_window_seconds: 3600,
                        retroactive_freeze_seconds: 1800,
                    },
                    canonical_rulebook: format!(
                        "GitHub release version tracker for {}/{}",
                        owner, repo
                    ),
                }
            }

            Self::SportsMatch {
                event_id,
                target_team,
                condition,
                kickoff_time,
            } => {
                let primary_url = format!(
                    "https://site.api.espn.com/apis/site/v2/sports/soccer/eng.1/scoreboard/{}",
                    event_id
                );

                let question = format!(
                    "Will {} satisfy '{}' in match {} (kickoff {} UTC)?",
                    target_team,
                    condition,
                    event_id,
                    kickoff_time.format("%Y-%m-%d %H:%M")
                );

                let ast = Expression::Compare {
                    op: ComparisonOperator::Equals,
                    left: Box::new(Expression::JsonPath {
                        path: "header.competitions[0].status.type.completed".to_string(),
                        target: Box::new(Expression::PayloadRoot),
                    }),
                    right: Box::new(Expression::Literal {
                        value: Value::Text("true".to_string()),
                    }),
                };

                CompiledMarketSpec {
                    question,
                    primary_source_url: primary_url,
                    secondary_source_urls: vec![],
                    ast_expression: ast,
                    temporal_rules: TemporalRules {
                        cutoff_time: kickoff_time.timestamp() + 7200, // 2 hours post kickoff
                        challenge_window_seconds: 1800,
                        retroactive_freeze_seconds: 900,
                    },
                    canonical_rulebook: format!(
                        "ESPN sports match status oracle for event {}",
                        event_id
                    ),
                }
            }
        }
    }
}
