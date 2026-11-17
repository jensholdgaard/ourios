//! Why a steward pass failed: a proposal it made did not commit (RFC 0065
//! §6.5, §6.7, §6.8). The next pass retries.

use std::fmt;

use openraft::errors::{ClientWriteError, RaftError};

use super::super::types::RaftTypes;

/// A membership change, flusher assignment, miner configuration or id
/// block the steward proposed did not commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StewardError(RaftError<RaftTypes, ClientWriteError<RaftTypes>>);

impl StewardError {
    /// The value of `error.type` it is reported under.
    #[must_use]
    pub fn error_type(&self) -> &'static str {
        match &self.0 {
            RaftError::APIError(ClientWriteError::ForwardToLeader(_)) => "not_leader",
            RaftError::APIError(ClientWriteError::LogEntryDiscarded(_)) => "leader_changed",
            RaftError::APIError(ClientWriteError::ChangeMembershipError(_)) => "membership_refused",
            RaftError::APIError(ClientWriteError::PreconditionFailed(_)) => "membership_changed",
            RaftError::Fatal(_) => "raft_stopped",
        }
    }
}

impl From<RaftError<RaftTypes, ClientWriteError<RaftTypes>>> for StewardError {
    fn from(error: RaftError<RaftTypes, ClientWriteError<RaftTypes>>) -> Self {
        Self(error)
    }
}

impl fmt::Display for StewardError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for StewardError {}

#[cfg(test)]
mod tests {
    use openraft::errors::{
        ChangeMembershipError, ClientWriteError, Fatal, ForwardToLeader, InProgress, RaftError,
    };

    use super::StewardError;
    use crate::replicated::raft::types::RaftTypes;

    fn api(error: ClientWriteError<RaftTypes>) -> StewardError {
        StewardError::from(RaftError::APIError(error))
    }

    #[test]
    fn each_cause_has_its_error_type() {
        let in_progress = InProgress {
            committed: None,
            membership_log_id: None,
        };
        let cases = [
            (
                api(ClientWriteError::ForwardToLeader(ForwardToLeader::empty())),
                "not_leader",
            ),
            (
                api(ClientWriteError::LogEntryDiscarded(ForwardToLeader::empty())),
                "leader_changed",
            ),
            (
                api(ClientWriteError::ChangeMembershipError(
                    ChangeMembershipError::InProgress(in_progress),
                )),
                "membership_refused",
            ),
            (
                StewardError::from(RaftError::Fatal(Fatal::Stopped)),
                "raft_stopped",
            ),
        ];
        for (error, expected) in cases {
            assert_eq!(error.error_type(), expected, "{error}");
        }
    }
}
