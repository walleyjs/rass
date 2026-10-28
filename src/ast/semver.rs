use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use thiserror::Error;

#[derive(Error, Debug, PartialEq, Eq, Clone)]
pub enum SemVerError {
    #[error("Invalid SemVer string '{0}': expected format [v]MAJOR.MINOR.PATCH[-PRERELEASE]")]
    InvalidFormat(String),
    #[error("Integer parse error in SemVer component '{component}': {source}")]
    ParseIntError {
        component: String,
        source: std::num::ParseIntError,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParsedSemVer {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub pre_release: Option<String>,
}

impl ParsedSemVer {
    pub fn parse(s: &str) -> Result<Self, SemVerError> {
        let trimmed = s.trim().trim_start_matches('v').trim_start_matches('V');
        let (ver_part, pre_part) = match trimmed.split_once('-') {
            Some((v, p)) => (v, Some(p.to_string())),
            None => (trimmed, None),
        };

        let parts: Vec<&str> = ver_part.split('.').collect();
        if parts.is_empty() || parts.len() > 3 {
            return Err(SemVerError::InvalidFormat(s.to_string()));
        }

        let major = parts[0]
            .parse::<u64>()
            .map_err(|e| SemVerError::ParseIntError {
                component: parts[0].to_string(),
                source: e,
            })?;

        let minor = if parts.len() > 1 {
            parts[1]
                .parse::<u64>()
                .map_err(|e| SemVerError::ParseIntError {
                    component: parts[1].to_string(),
                    source: e,
                })?
        } else {
            0
        };

        let patch = if parts.len() > 2 {
            parts[2]
                .parse::<u64>()
                .map_err(|e| SemVerError::ParseIntError {
                    component: parts[2].to_string(),
                    source: e,
                })?
        } else {
            0
        };

        Ok(Self {
            major,
            minor,
            patch,
            pre_release: pre_part,
        })
    }
}

impl PartialOrd for ParsedSemVer {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ParsedSemVer {
    fn cmp(&self, other: &Self) -> Ordering {
        match self.major.cmp(&other.major) {
            Ordering::Equal => {}
            non_eq => return non_eq,
        }
        match self.minor.cmp(&other.minor) {
            Ordering::Equal => {}
            non_eq => return non_eq,
        }
        match self.patch.cmp(&other.patch) {
            Ordering::Equal => {}
            non_eq => return non_eq,
        }
        // Pre-release versions have lower precedence than normal versions: 1.0.0-alpha < 1.0.0
        match (&self.pre_release, &other.pre_release) {
            (None, None) => Ordering::Equal,
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (Some(a), Some(b)) => a.cmp(b),
        }
    }
}
