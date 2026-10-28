pub mod semver;

use regex::Regex;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::cmp::Ordering;
use thiserror::Error;

pub use semver::ParsedSemVer;

#[derive(Error, Debug, PartialEq, Clone)]
pub enum AstError {
    #[error("Type mismatch: expected {expected}, found {found}")]
    TypeMismatch { expected: String, found: String },
    #[error("Evaluation failed: {0}")]
    EvaluationError(String),
    #[error("JsonPath resolution failed: {0}")]
    JsonPathError(String),
    #[error("Regex execution failed: {0}")]
    RegexError(String),
    #[error("HTML/DOM extraction failed: {0}")]
    DomExtractionError(String),
    #[error("SchemaOrg JSON-LD extraction failed: {0}")]
    SchemaOrgError(String),
    #[error("Decimal parse error: '{raw}' is not a valid fixed-point decimal: {source}")]
    DecimalParseError {
        raw: String,
        source: rust_decimal::Error,
    },
    #[error("SemVer parse error: '{raw}': {source}")]
    SemVerParseError {
        raw: String,
        source: semver::SemVerError,
    },
    #[error("Consensus failed: agreement ratio {actual} < required {required}")]
    ConsensusFailed { actual: Decimal, required: Decimal },
    #[error("Empty multi-witness strategy list")]
    EmptyWitnessList,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value")]
pub enum Value {
    Decimal(Decimal),
    SemVer(ParsedSemVer),
    Text(String),
    Boolean(bool),
    Bytes(Vec<u8>),
    Null,
}

impl Value {
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Decimal(_) => "Decimal",
            Self::SemVer(_) => "SemVer",
            Self::Text(_) => "Text",
            Self::Boolean(_) => "Boolean",
            Self::Bytes(_) => "Bytes",
            Self::Null => "Null",
        }
    }

    pub fn as_text(&self) -> Result<&str, AstError> {
        match self {
            Self::Text(s) => Ok(s.as_str()),
            other => Err(AstError::TypeMismatch {
                expected: "Text".into(),
                found: other.type_name().into(),
            }),
        }
    }

    pub fn as_decimal(&self) -> Result<Decimal, AstError> {
        match self {
            Self::Decimal(d) => Ok(*d),
            Self::Text(s) => s
                .trim()
                .parse::<Decimal>()
                .map_err(|e| AstError::DecimalParseError {
                    raw: s.clone(),
                    source: e,
                }),
            other => Err(AstError::TypeMismatch {
                expected: "Decimal".into(),
                found: other.type_name().into(),
            }),
        }
    }

    pub fn as_semver(&self) -> Result<ParsedSemVer, AstError> {
        match self {
            Self::SemVer(sv) => Ok(sv.clone()),
            Self::Text(s) => ParsedSemVer::parse(s).map_err(|e| AstError::SemVerParseError {
                raw: s.clone(),
                source: e,
            }),
            other => Err(AstError::TypeMismatch {
                expected: "SemVer".into(),
                found: other.type_name().into(),
            }),
        }
    }

    pub fn as_bool(&self) -> Result<bool, AstError> {
        match self {
            Self::Boolean(b) => Ok(*b),
            other => Err(AstError::TypeMismatch {
                expected: "Boolean".into(),
                found: other.type_name().into(),
            }),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ComparisonOperator {
    GreaterThan,
    GreaterThanOrEqual,
    LessThan,
    LessThanOrEqual,
    Equals,
    NotEquals,
    Contains,
    NotContains,
    RegexMatch,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvaluationContext {
    pub raw_body: String,
}

impl EvaluationContext {
    pub fn new(body: impl Into<String>) -> Self {
        Self {
            raw_body: body.into(),
        }
    }
}

/// Pure, side-effect-free Abstract Syntax Tree for deterministic market resolution
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum Expression {
    Literal {
        value: Value,
    },
    PayloadRoot,
    JsonPath {
        path: String,
        target: Box<Expression>,
    },
    RegexMatch {
        pattern: String,
        capture_group: usize,
        target: Box<Expression>,
    },
    DomSelector {
        tag: String,
        attr_name: Option<String>,
        attr_value: Option<String>,
        target_attribute: Option<String>,
        target: Box<Expression>,
    },
    SchemaOrgJsonLd {
        target_type: String,
        property: String,
        target: Box<Expression>,
    },
    ParseDecimal {
        target: Box<Expression>,
    },
    ParseSemVer {
        target: Box<Expression>,
    },
    Compare {
        op: ComparisonOperator,
        left: Box<Expression>,
        right: Box<Expression>,
    },
    MultiWitnessConsensus {
        strategies: Vec<Expression>,
        min_agreement_ratio: Decimal,
    },
    LogicalAnd {
        operands: Vec<Expression>,
    },
    LogicalOr {
        operands: Vec<Expression>,
    },
    Not {
        operand: Box<Expression>,
    },
}

impl Expression {
    /// Pure, side-effect-free recursive evaluation
    pub fn evaluate(&self, ctx: &EvaluationContext) -> Result<Value, AstError> {
        match self {
            Self::Literal { value } => Ok(value.clone()),

            Self::PayloadRoot => Ok(Value::Text(ctx.raw_body.clone())),

            Self::JsonPath { path, target } => {
                let target_val = target.evaluate(ctx)?;
                let text = target_val.as_text()?;
                let json: JsonValue = serde_json::from_str(text)
                    .map_err(|e| AstError::JsonPathError(format!("Invalid JSON: {}", e)))?;
                let resolved = Self::resolve_json_path(&json, path)?;
                Ok(Value::Text(resolved))
            }

            Self::RegexMatch {
                pattern,
                capture_group,
                target,
            } => {
                let target_val = target.evaluate(ctx)?;
                let text = target_val.as_text()?;
                let re = Regex::new(pattern)
                    .map_err(|e| AstError::RegexError(format!("Invalid regex: {}", e)))?;
                if let Some(caps) = re.captures(text) {
                    caps.get(*capture_group)
                        .map(|m| Value::Text(m.as_str().to_string()))
                        .ok_or_else(|| {
                            AstError::RegexError(format!(
                                "Capture group {} not found",
                                capture_group
                            ))
                        })
                } else {
                    Err(AstError::RegexError(format!(
                        "Pattern '{}' did not match input",
                        pattern
                    )))
                }
            }

            Self::DomSelector {
                tag,
                attr_name,
                attr_value,
                target_attribute,
                target,
            } => {
                let target_val = target.evaluate(ctx)?;
                let text = target_val.as_text()?;
                let extracted = Self::resolve_dom_selector(
                    text,
                    tag,
                    attr_name.as_deref(),
                    attr_value.as_deref(),
                    target_attribute.as_deref(),
                )?;
                Ok(Value::Text(extracted))
            }

            Self::SchemaOrgJsonLd {
                target_type,
                property,
                target,
            } => {
                let target_val = target.evaluate(ctx)?;
                let text = target_val.as_text()?;
                let extracted = Self::resolve_schema_org(text, target_type, property)?;
                Ok(Value::Text(extracted))
            }

            Self::ParseDecimal { target } => {
                let target_val = target.evaluate(ctx)?;
                let dec = target_val.as_decimal()?;
                Ok(Value::Decimal(dec))
            }

            Self::ParseSemVer { target } => {
                let target_val = target.evaluate(ctx)?;
                let sv = target_val.as_semver()?;
                Ok(Value::SemVer(sv))
            }

            Self::Compare { op, left, right } => {
                let left_val = left.evaluate(ctx)?;
                let right_val = right.evaluate(ctx)?;
                let result = Self::evaluate_comparison(*op, &left_val, &right_val)?;
                Ok(Value::Boolean(result))
            }

            Self::MultiWitnessConsensus {
                strategies,
                min_agreement_ratio,
            } => {
                if strategies.is_empty() {
                    return Err(AstError::EmptyWitnessList);
                }

                let mut extracted = Vec::with_capacity(strategies.len());
                for s in strategies {
                    extracted.push(s.evaluate(ctx)?);
                }

                // Count votes for majority value
                let mut counts: Vec<(Value, usize)> = Vec::new();
                for val in &extracted {
                    if let Some(entry) = counts.iter_mut().find(|(v, _)| v == val) {
                        entry.1 += 1;
                    } else {
                        counts.push((val.clone(), 1));
                    }
                }

                counts.sort_by_key(|a| std::cmp::Reverse(a.1));
                let (winner_val, winner_count) = &counts[0];

                let total_witnesses = Decimal::from(strategies.len() as u64);
                let actual_agreement = Decimal::from(*winner_count as u64) / total_witnesses;

                if actual_agreement >= *min_agreement_ratio {
                    Ok(winner_val.clone())
                } else {
                    Err(AstError::ConsensusFailed {
                        actual: actual_agreement,
                        required: *min_agreement_ratio,
                    })
                }
            }

            Self::LogicalAnd { operands } => {
                for op in operands {
                    let val = op.evaluate(ctx)?;
                    if !val.as_bool()? {
                        return Ok(Value::Boolean(false));
                    }
                }
                Ok(Value::Boolean(true))
            }

            Self::LogicalOr { operands } => {
                for op in operands {
                    let val = op.evaluate(ctx)?;
                    if val.as_bool()? {
                        return Ok(Value::Boolean(true));
                    }
                }
                Ok(Value::Boolean(false))
            }

            Self::Not { operand } => {
                let val = operand.evaluate(ctx)?;
                Ok(Value::Boolean(!val.as_bool()?))
            }
        }
    }

    fn evaluate_comparison(
        op: ComparisonOperator,
        left: &Value,
        right: &Value,
    ) -> Result<bool, AstError> {
        match (left, right) {
            (Value::Decimal(l), Value::Decimal(r)) => {
                let ord = l.cmp(r);
                Ok(match op {
                    ComparisonOperator::GreaterThan => ord == Ordering::Greater,
                    ComparisonOperator::GreaterThanOrEqual => {
                        ord == Ordering::Greater || ord == Ordering::Equal
                    }
                    ComparisonOperator::LessThan => ord == Ordering::Less,
                    ComparisonOperator::LessThanOrEqual => {
                        ord == Ordering::Less || ord == Ordering::Equal
                    }
                    ComparisonOperator::Equals => ord == Ordering::Equal,
                    ComparisonOperator::NotEquals => ord != Ordering::Equal,
                    _ => {
                        return Err(AstError::EvaluationError(format!(
                            "Operator {:?} unsupported for Decimal comparison",
                            op
                        )))
                    }
                })
            }
            (Value::SemVer(l), Value::SemVer(r)) => {
                let ord = l.cmp(r);
                Ok(match op {
                    ComparisonOperator::GreaterThan => ord == Ordering::Greater,
                    ComparisonOperator::GreaterThanOrEqual => {
                        ord == Ordering::Greater || ord == Ordering::Equal
                    }
                    ComparisonOperator::LessThan => ord == Ordering::Less,
                    ComparisonOperator::LessThanOrEqual => {
                        ord == Ordering::Less || ord == Ordering::Equal
                    }
                    ComparisonOperator::Equals => ord == Ordering::Equal,
                    ComparisonOperator::NotEquals => ord != Ordering::Equal,
                    _ => {
                        return Err(AstError::EvaluationError(format!(
                            "Operator {:?} unsupported for SemVer comparison",
                            op
                        )))
                    }
                })
            }
            (Value::Text(l), Value::Text(r)) => Ok(match op {
                ComparisonOperator::Equals => l == r,
                ComparisonOperator::NotEquals => l != r,
                ComparisonOperator::Contains => l.contains(r),
                ComparisonOperator::NotContains => !l.contains(r),
                ComparisonOperator::RegexMatch => {
                    let re = Regex::new(r)
                        .map_err(|e| AstError::RegexError(format!("Invalid regex: {}", e)))?;
                    re.is_match(l)
                }
                _ => {
                    return Err(AstError::EvaluationError(format!(
                        "Operator {:?} unsupported for Text comparison without numeric parsing",
                        op
                    )))
                }
            }),
            (Value::Boolean(l), Value::Boolean(r)) => Ok(match op {
                ComparisonOperator::Equals => l == r,
                ComparisonOperator::NotEquals => l != r,
                _ => {
                    return Err(AstError::EvaluationError(format!(
                        "Operator {:?} unsupported for Boolean",
                        op
                    )))
                }
            }),
            _ => Err(AstError::TypeMismatch {
                expected: format!("matching types for left ({})", left.type_name()),
                found: right.type_name().into(),
            }),
        }
    }

    fn resolve_json_path(root: &JsonValue, path: &str) -> Result<String, AstError> {
        let clean = path.trim_start_matches("$.");
        let mut curr = root;

        for part in clean.split('.') {
            if part.is_empty() {
                continue;
            }

            if let Some((field, idx_str)) = part.split_once('[') {
                if !field.is_empty() {
                    curr = curr.get(field).ok_or_else(|| {
                        AstError::JsonPathError(format!("Field '{}' not found", field))
                    })?;
                }
                let idx: usize = idx_str
                    .trim_end_matches(']')
                    .parse()
                    .map_err(|_| AstError::JsonPathError(format!("Invalid index in '{}'", part)))?;
                curr = curr.get(idx).ok_or_else(|| {
                    AstError::JsonPathError(format!("Index {} not found in '{}'", idx, part))
                })?;
            } else {
                curr = curr.get(part).ok_or_else(|| {
                    AstError::JsonPathError(format!("Field '{}' not found", part))
                })?;
            }
        }

        match curr {
            JsonValue::String(s) => Ok(s.clone()),
            JsonValue::Number(n) => Ok(n.to_string()),
            JsonValue::Bool(b) => Ok(b.to_string()),
            JsonValue::Null => Ok("null".to_string()),
            _ => Ok(curr.to_string()),
        }
    }

    fn resolve_dom_selector(
        html: &str,
        tag: &str,
        attr_name: Option<&str>,
        attr_value: Option<&str>,
        target_attribute: Option<&str>,
    ) -> Result<String, AstError> {
        let pattern = if let (Some(name), Some(val)) = (attr_name, attr_value) {
            format!(
                r#"<{}\s+[^>]*{}="([^"]*{}[^"]*)"[^>]*>([^<]*)</{}>"#,
                regex::escape(tag),
                regex::escape(name),
                regex::escape(val),
                regex::escape(tag)
            )
        } else {
            format!(
                r#"<{}\b[^>]*>([^<]*)</{}>"#,
                regex::escape(tag),
                regex::escape(tag)
            )
        };

        let re = Regex::new(&pattern).map_err(|e| AstError::DomExtractionError(e.to_string()))?;
        if let Some(caps) = re.captures(html) {
            if let Some(target_attr) = target_attribute {
                let attr_pat = format!(r#"{}="([^"]*)""#, regex::escape(target_attr));
                let attr_re = Regex::new(&attr_pat)
                    .map_err(|e| AstError::DomExtractionError(e.to_string()))?;
                if let Some(c) = attr_re.captures(&caps[0]) {
                    return Ok(c[1].to_string());
                }
            } else {
                let text_group = if attr_name.is_some() && attr_value.is_some() {
                    2
                } else {
                    1
                };
                if let Some(m) = caps.get(text_group) {
                    return Ok(m.as_str().trim().to_string());
                }
            }
        }

        Err(AstError::DomExtractionError(format!(
            "DOM selector for tag '{}' not matched in document",
            tag
        )))
    }

    fn resolve_schema_org(
        html: &str,
        target_type: &str,
        property: &str,
    ) -> Result<String, AstError> {
        let re = Regex::new(
            r#"(?s)<script\s+[^>]*type=["']application/ld\+json["'][^>]*>(.*?)</script>"#,
        )
        .map_err(|e| AstError::SchemaOrgError(e.to_string()))?;

        for caps in re.captures_iter(html) {
            if let Some(script_content) = caps.get(1) {
                if let Ok(json) = serde_json::from_str::<JsonValue>(script_content.as_str().trim())
                {
                    if let Some(res) = Self::search_schema_value(&json, target_type, property) {
                        return Ok(res);
                    }
                }
            }
        }

        Err(AstError::SchemaOrgError(format!(
            "SchemaOrg type '{}' with property '{}' not found",
            target_type, property
        )))
    }

    fn search_schema_value(json: &JsonValue, target_type: &str, property: &str) -> Option<String> {
        match json {
            JsonValue::Object(map) => {
                let is_type = map.get("@type").and_then(|t| t.as_str()).is_some_and(|t| {
                    t.eq_ignore_ascii_case(target_type) || t.ends_with(&format!("/{}", target_type))
                });

                if is_type {
                    if let Some(val) = map.get(property) {
                        return match val {
                            JsonValue::String(s) => Some(s.clone()),
                            JsonValue::Number(n) => Some(n.to_string()),
                            JsonValue::Bool(b) => Some(b.to_string()),
                            _ => Some(val.to_string()),
                        };
                    }
                }

                for v in map.values() {
                    if let Some(res) = Self::search_schema_value(v, target_type, property) {
                        return Some(res);
                    }
                }
            }
            JsonValue::Array(arr) => {
                for item in arr {
                    if let Some(res) = Self::search_schema_value(item, target_type, property) {
                        return Some(res);
                    }
                }
            }
            _ => {}
        }
        None
    }
}
