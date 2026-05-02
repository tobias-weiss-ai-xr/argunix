use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("unknown status: {0}")]
pub struct ParseStatusError(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Queued,
    Running,
    Cached,
    Success,
    Failure,
    Cancelled,
    Interrupted,
    SkippedNoBuilder,
}

impl JobStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            JobStatus::Queued => "queued",
            JobStatus::Running => "running",
            JobStatus::Cached => "cached",
            JobStatus::Success => "success",
            JobStatus::Failure => "failure",
            JobStatus::Cancelled => "cancelled",
            JobStatus::Interrupted => "interrupted",
            JobStatus::SkippedNoBuilder => "skipped_no_builder",
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            JobStatus::Cached
                | JobStatus::Success
                | JobStatus::Failure
                | JobStatus::Cancelled
                | JobStatus::SkippedNoBuilder
        )
    }

    pub fn is_success(self) -> bool {
        matches!(self, JobStatus::Cached | JobStatus::Success)
    }

    pub fn is_failure(self) -> bool {
        matches!(self, JobStatus::Failure)
    }
}

impl fmt::Display for JobStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for JobStatus {
    type Err = ParseStatusError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "queued" => Ok(JobStatus::Queued),
            "running" => Ok(JobStatus::Running),
            "cached" => Ok(JobStatus::Cached),
            "success" => Ok(JobStatus::Success),
            "failure" => Ok(JobStatus::Failure),
            "cancelled" => Ok(JobStatus::Cancelled),
            "interrupted" => Ok(JobStatus::Interrupted),
            "skipped_no_builder" => Ok(JobStatus::SkippedNoBuilder),
            _ => Err(ParseStatusError(s.to_string())),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvalStatus {
    Queued,
    Evaluating,
    EvaluationFailed,
    Building,
    Done,
    Cancelled,
}

impl EvalStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            EvalStatus::Queued => "queued",
            EvalStatus::Evaluating => "evaluating",
            EvalStatus::EvaluationFailed => "evaluation_failed",
            EvalStatus::Building => "building",
            EvalStatus::Done => "done",
            EvalStatus::Cancelled => "cancelled",
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            EvalStatus::EvaluationFailed | EvalStatus::Done | EvalStatus::Cancelled
        )
    }
}

impl fmt::Display for EvalStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for EvalStatus {
    type Err = ParseStatusError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "queued" => Ok(EvalStatus::Queued),
            "evaluating" => Ok(EvalStatus::Evaluating),
            "evaluation_failed" => Ok(EvalStatus::EvaluationFailed),
            "building" => Ok(EvalStatus::Building),
            "done" => Ok(EvalStatus::Done),
            "cancelled" => Ok(EvalStatus::Cancelled),
            _ => Err(ParseStatusError(s.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_status_terminals() {
        assert!(!JobStatus::Queued.is_terminal());
        assert!(!JobStatus::Running.is_terminal());
        assert!(!JobStatus::Interrupted.is_terminal());
        assert!(JobStatus::Success.is_terminal());
        assert!(JobStatus::Failure.is_terminal());
        assert!(JobStatus::Cached.is_terminal());
        assert!(JobStatus::Cancelled.is_terminal());
        assert!(JobStatus::SkippedNoBuilder.is_terminal());
    }

    #[test]
    fn cached_counts_as_success() {
        assert!(JobStatus::Cached.is_success());
        assert!(JobStatus::Success.is_success());
        assert!(!JobStatus::Failure.is_success());
        assert!(!JobStatus::Cancelled.is_success());
    }

    #[test]
    fn job_status_string_round_trip() {
        for s in [
            JobStatus::Queued,
            JobStatus::Running,
            JobStatus::Cached,
            JobStatus::Success,
            JobStatus::Failure,
            JobStatus::Cancelled,
            JobStatus::Interrupted,
            JobStatus::SkippedNoBuilder,
        ] {
            assert_eq!(JobStatus::from_str(s.as_str()).unwrap(), s);
        }
    }

    #[test]
    fn eval_status_string_round_trip() {
        for s in [
            EvalStatus::Queued,
            EvalStatus::Evaluating,
            EvalStatus::EvaluationFailed,
            EvalStatus::Building,
            EvalStatus::Done,
            EvalStatus::Cancelled,
        ] {
            assert_eq!(EvalStatus::from_str(s.as_str()).unwrap(), s);
        }
    }

    #[test]
    fn unknown_status_errors() {
        assert!(JobStatus::from_str("nonsense").is_err());
        assert!(EvalStatus::from_str("nonsense").is_err());
    }
}
