//! How an ingest fails, and how each failure is classified for the
//! transports (RFC 0018 §3.2, RFC 0026 §3.2, RFC 0052 §3.3).

use ourios_core::tenant::TenantId;
use ourios_serving::AuthBinding;

/// Failure ingesting an export. Tenant-resolution failures reject the
/// whole batch (RFC0003.4); WAL failures mean the batch is not acked
/// (`[§3.4]`).
#[derive(Debug)]
#[non_exhaustive]
pub enum ReceiveError {
    /// The selected tenant falls outside the authenticated token's
    /// allowed set — the whole batch is denied before any WAL work
    /// (RFC 0026 §3.2, `PERMISSION_DENIED` / 403).
    TenantDenied {
        /// The rejecting token's audit/metric label (never the value) —
        /// what the §3.4 rejection audit event will carry.
        token_name: String,
        /// The offending selected tenant.
        tenant: ourios_core::tenant::TenantId,
    },
    /// The tenant could not be framed into the WAL payload (RFC 0046
    /// §3.3) — unreachable for a transport-validated selector; surfaced
    /// rather than unwrapped.
    TenantFrame(ourios_wal::TenantBatchError),
    /// Appending the `OtlpBatch` frame to the WAL failed.
    WalAppend(ourios_wal::AppendError),
    /// Fsyncing the WAL failed — the batch must not be acked.
    WalSync(ourios_wal::SyncError),
    /// The replicated backend's group did not commit the export (RFC 0065
    /// §6.3) — the batch must not be acked.
    Replication(crate::replicated::write::WriteError),
    /// The group's leader refused the forwarded export before appending
    /// it; returned to the client as the leader gave it (RFC 0065 §6.8).
    Forwarded(crate::replicated::raft::transport::Rejected),
}

/// How a transport classifies a failed batch for the `ingest logs` server
/// span: the `error.type` it fails with, or `None` for a status that is not
/// a server error under that transport's convention.
pub(crate) type SpanError = fn(&ReceiveError) -> Option<String>;

/// The transport-agnostic [`SpanError`]: the class's
/// [`IngestFailure::span_error_type`].
pub(crate) fn span_error(error: &ReceiveError) -> Option<String> {
    IngestFailure::classify(error)
        .span_error_type()
        .map(str::to_owned)
}

/// The transport-agnostic classification of a [`ReceiveError`] —
/// one exhaustive match instead of one per transport (epic #745
/// wave 2; this replaced twin 4-arm mappers in `http.rs`/`grpc.rs`,
/// which now adapt these four outcomes to their status vocabularies).
/// A future `ReceiveError` variant breaks the build HERE, forcing one
/// retryable-vs-not decision instead of two independently-drifting
/// ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum IngestFailure {
    /// An authenticated caller writing outside its tenant set — a
    /// permanent authz rejection, whole batch, pre-WAL (RFC 0026
    /// §3.2). HTTP 403 / gRPC `PERMISSION_DENIED`.
    Denied,
    /// The batch exceeds the WAL's frame bound; the client must
    /// shrink it. HTTP 413 / gRPC `RESOURCE_EXHAUSTED` without
    /// `RetryInfo`, as for an Export over the listener's limit (RFC 0061
    /// §3.2).
    TooLarge,
    /// The credential is missing or invalid, as a forwarded export's
    /// leader found it: the direct path refuses it before ingest. HTTP 401
    /// / gRPC `UNAUTHENTICATED`.
    Unauthenticated,
    /// Durability is temporarily unavailable; retryable. HTTP 503 /
    /// gRPC `UNAVAILABLE`.
    Unavailable,
    /// **Server-terminal, client-retryable** — RFC 0018 §3.2's third
    /// class, which RFC 0052 §3.3 adds. The WAL's rotation retry budget
    /// is exhausted: no delay fixes the node, it needs an operator, and
    /// the only exit today is a restart (#791).
    ///
    /// Still HTTP 503 / gRPC `UNAVAILABLE`, deliberately and for the
    /// reason §3.2 gives: the batch was never acked, and every
    /// non-retryable OTLP status also instructs the client to *drop*
    /// it, which would turn a terminal node into silent data loss.
    /// OTLP's retryable axis is about the data, and this batch is
    /// valid — it will be accepted once the node is cleared. What
    /// changes is the message, which names the state instead of
    /// implying a blip, and that no retry hint is carried: the server
    /// schedules no retry of its own, so a conforming client backs off
    /// exponentially.
    ServerTerminal,
    /// Unreachable for a transport-validated selector; our bug.
    /// HTTP 500 / gRPC `INTERNAL`.
    Internal,
}

impl IngestFailure {
    /// Classify `error` into the transport-agnostic outcome.
    #[must_use]
    pub(super) fn classify(error: &ReceiveError) -> Self {
        match error {
            ReceiveError::TenantDenied { .. } => Self::Denied,
            ReceiveError::WalAppend(ourios_wal::AppendError::TooLarge { .. })
            | ReceiveError::Replication(crate::replicated::write::WriteError::TooLarge {
                ..
            }) => Self::TooLarge,
            // RFC 0052 §3.3's narrow reclassification: only the
            // *terminal* rotation state leaves the transient class. A
            // rotation still inside its retry budget genuinely is
            // transient — a later append can succeed — and an ordinary
            // append or fsync I/O failure never reaches this arm at all.
            ReceiveError::WalAppend(ourios_wal::AppendError::RotationTerminal(_))
            | ReceiveError::WalSync(ourios_wal::SyncError::RotationTerminal(_)) => {
                Self::ServerTerminal
            }
            ReceiveError::WalAppend(_)
            | ReceiveError::WalSync(_)
            | ReceiveError::Replication(_) => Self::Unavailable,
            ReceiveError::TenantFrame(_) => Self::Internal,
            ReceiveError::Forwarded(rejected) => Self::of_rejection(rejected.rejection),
        }
    }

    /// A leader's rejection's class, as the client would have got it there.
    fn of_rejection(rejection: crate::replicated::raft::transport::Rejection) -> Self {
        use crate::replicated::raft::transport::Rejection;
        match rejection {
            Rejection::Unauthenticated => Self::Unauthenticated,
            Rejection::Unavailable => Self::Unavailable,
            Rejection::PermissionDenied => Self::Denied,
            Rejection::TooLarge => Self::TooLarge,
        }
    }

    /// The low-cardinality `error.type` of the class.
    #[must_use]
    pub(super) fn error_type(self) -> &'static str {
        match self {
            Self::Denied => "permission_denied",
            Self::Unauthenticated => "unauthenticated",
            Self::TooLarge => "too_large",
            Self::Unavailable => "unavailable",
            Self::ServerTerminal => "server_terminal",
            Self::Internal => "internal",
        }
    }

    /// The `error.type` the batch span fails with, or `None` for a
    /// rejection that is the client's fault: an `ingest logs` span is a
    /// server span, which semconv leaves unset on a 4xx / client-error
    /// status (HTTP 403 / 413, gRPC `PERMISSION_DENIED` /
    /// `RESOURCE_EXHAUSTED`). The gRPC server convention counts UNAVAILABLE
    /// and INTERNAL, the codes the other classes map to, as errors.
    #[must_use]
    pub(super) fn span_error_type(self) -> Option<&'static str> {
        match self {
            Self::Denied | Self::Unauthenticated | Self::TooLarge => None,
            Self::Unavailable | Self::ServerTerminal | Self::Internal => Some(self.error_type()),
        }
    }
}

impl std::fmt::Display for ReceiveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // The wire message names the tenant, not the token: the caller
            // knows its own credential, and the token's label belongs to
            // the operator's audit surface (RFC 0026 §3.4), not the
            // client's error text.
            Self::TenantDenied { tenant, .. } => write!(
                f,
                "tenant `{}` is outside the authenticated token's allowed \
                 tenant set (RFC 0026 §3.2)",
                tenant.as_str(),
            ),
            Self::TenantFrame(e) => write!(f, "{e}"),
            Self::WalAppend(e) => write!(f, "{e}"),
            Self::WalSync(e) => write!(f, "{e}"),
            Self::Replication(e) => write!(f, "{e}"),
            Self::Forwarded(rejected) => f.write_str(&rejected.message),
        }
    }
}

impl std::error::Error for ReceiveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::TenantDenied { .. } | Self::Forwarded(_) => None,
            Self::TenantFrame(e) => Some(e),
            Self::WalAppend(e) => Some(e),
            Self::WalSync(e) => Some(e),
            Self::Replication(e) => Some(e),
        }
    }
}

/// Enforce the §3.2 per-batch tenant binding: the out-of-band selected
/// `tenant` (RFC 0046 §3.1) must fall inside the binding's write set.
///
/// Runs before the WAL append *and* before materialisation, so a denied
/// batch does no ingest work at all. Lives with [`ReceiveError`] — the
/// error it raises — since RFC 0051 moved the resolver to
/// `ourios-serving`; the binding *decision* stays on the ingest side.
///
/// # Errors
///
/// [`ReceiveError::TenantDenied`] when `tenant` is outside the set — the
/// whole batch is rejected (`PERMISSION_DENIED` / 403).
pub(crate) fn check_binding(tenant: &TenantId, binding: &AuthBinding) -> Result<(), ReceiveError> {
    if binding.may_write(tenant.as_str()) {
        Ok(())
    } else {
        Err(ReceiveError::TenantDenied {
            token_name: binding.token_name().to_string(),
            tenant: tenant.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A leader's rejection of a forwarded export classifies as it would
    /// have there: an unauthenticated one as the client's fault, so the
    /// server span stays unset, and its text is the leader's message.
    #[test]
    fn a_forwarded_rejection_keeps_its_class_and_message() {
        use crate::replicated::raft::transport::{Rejected, Rejection};
        let forwarded = |rejection| {
            ReceiveError::Forwarded(Rejected {
                rejection,
                error_type: String::new(),
                message: "the leader's words".to_owned(),
                details: Vec::new(),
            })
        };
        for (rejection, class) in [
            (Rejection::Unauthenticated, IngestFailure::Unauthenticated),
            (Rejection::Unavailable, IngestFailure::Unavailable),
            (Rejection::PermissionDenied, IngestFailure::Denied),
            (Rejection::TooLarge, IngestFailure::TooLarge),
        ] {
            let error = forwarded(rejection);
            assert_eq!(IngestFailure::classify(&error), class, "{rejection:?}");
            assert_eq!(error.to_string(), "the leader's words");
        }
        let unauthenticated = IngestFailure::Unauthenticated;
        assert_eq!(unauthenticated.error_type(), "unauthenticated");
        assert_eq!(unauthenticated.span_error_type(), None);
    }

    /// The classifier exists to force "one retryable-vs-not decision"
    /// (see its doc comment). It used to fold a permanently quiesced
    /// WAL into the transient bucket, so a node only a restart could
    /// fix kept answering "retry later" forever (#791). RFC 0052 §3.3
    /// made the refusal *bounded*, so the rule is now terminal-only:
    /// the state that leaves the transient class is the exhausted
    /// budget, and nothing else.
    mod terminal_vs_transient {
        use super::super::{IngestFailure, ReceiveError};
        use ourios_wal::{AppendError, RotationFault, RotationSite, SyncError};

        fn fault(attempts: u32) -> RotationFault {
            RotationFault::new(
                RotationSite::CloseSync,
                &std::io::Error::other("io"),
                attempts,
                3,
            )
        }

        /// The state #794 called wedged. It is reached by exhausting the
        /// budget now, and every append after it carries this variant.
        #[test]
        fn a_terminal_rotation_state_classifies_server_terminal_not_transient() {
            assert_eq!(
                IngestFailure::classify(&ReceiveError::WalAppend(AppendError::RotationTerminal(
                    fault(3),
                ))),
                IngestFailure::ServerTerminal,
            );
        }

        /// The append whose own rotation spent the last unit of the
        /// budget is already terminal: every later append is refused.
        /// #794 classified it transient, which advised a retry on the
        /// one request that had just made retrying useless — and the
        /// *sync* surface reaches the same state, so it must classify
        /// the same way.
        #[test]
        fn the_append_and_the_sync_that_exhaust_the_budget_are_both_terminal() {
            for error in [
                ReceiveError::WalAppend(AppendError::RotationTerminal(fault(3))),
                ReceiveError::WalSync(SyncError::RotationTerminal(fault(3))),
            ] {
                assert_eq!(
                    IngestFailure::classify(&error),
                    IngestFailure::ServerTerminal
                );
            }
        }

        /// The half #794 got wrong once §3.3 lands: a rotation failure
        /// still inside its budget genuinely is transient, because a
        /// later append can succeed. Classifying it terminal would tell
        /// an operator to intervene on a blip.
        #[test]
        fn a_rotation_still_inside_its_budget_stays_transient() {
            for error in [
                ReceiveError::WalAppend(AppendError::RotationRetrying(fault(1))),
                ReceiveError::WalSync(SyncError::RotationRetrying(fault(1))),
            ] {
                assert_eq!(IngestFailure::classify(&error), IngestFailure::Unavailable);
            }
        }

        /// The batch span is a server span: the server's own failures
        /// mark it `Error`, a client-fault rejection leaves it unset.
        #[test]
        fn only_server_faults_fail_the_batch_span() {
            assert_eq!(IngestFailure::Denied.span_error_type(), None);
            assert_eq!(IngestFailure::TooLarge.span_error_type(), None);
            assert_eq!(
                IngestFailure::Unavailable.span_error_type(),
                Some("unavailable")
            );
            assert_eq!(
                IngestFailure::ServerTerminal.span_error_type(),
                Some("server_terminal")
            );
            assert_eq!(IngestFailure::Internal.span_error_type(), Some("internal"));
        }

        /// A real WAL whose idle timer spent the budget on an owed
        /// directory fsync, then whose flush's data sync fails on the
        /// same disk: the flush must still classify terminal, not as
        /// the ordinary sync failure a client should retry.
        #[test]
        fn a_failing_data_sync_behind_a_terminal_owed_dir_fsync_is_terminal() {
            use ourios_wal::{FrameKind, RotationFaults, RotationKind, Wal, WalConfig};

            let tmp = tempfile::TempDir::new().expect("temp");
            let mut wal = Wal::open(WalConfig {
                root: tmp.path().to_path_buf(),
                batch_window_ms: 20,
                segment_size_bytes: ourios_wal::MIN_SEGMENT_SIZE_BYTES,
                segment_age_secs: 600,
                housekeeping_secs: 60,
                max_unlinks_per_pass: ourios_wal::DEFAULT_MAX_UNLINKS_PER_PASS,
                rotation_retry_attempts: 2,
                macos_full_fsync: false,
            })
            .expect("open WAL");
            wal.append(FrameKind::OtlpBatch, b"seed").expect("seed");
            wal.arm_rotation_faults(RotationFaults::always(RotationSite::ParentFsync));
            wal.rotate(RotationKind::Owed)
                .expect_err("the rotation's parent fsync fails");
            wal.append(FrameKind::OtlpBatch, b"appended, never flushed")
                .expect("appends land in the installed segment");
            wal.rotate(RotationKind::Discretionary)
                .expect_err("the timer's discharge spends the budget");
            wal.arm_rotation_faults(
                RotationFaults::always(RotationSite::ParentFsync).and_failing_segment_sync(),
            );

            for _ in 0..3 {
                let error = ReceiveError::WalSync(wal.sync().expect_err("nothing is acked"));
                assert_eq!(
                    IngestFailure::classify(&error),
                    IngestFailure::ServerTerminal,
                    "{error}",
                );
            }
        }

        #[test]
        fn other_append_and_sync_failures_stay_transient() {
            assert_eq!(
                IngestFailure::classify(&ReceiveError::WalAppend(AppendError::Io {
                    op: "write",
                    source: std::io::Error::other("io"),
                })),
                IngestFailure::Unavailable,
            );
            assert_eq!(
                IngestFailure::classify(&ReceiveError::WalSync(SyncError::Io {
                    op: "fdatasync",
                    source: std::io::Error::other("io"),
                })),
                IngestFailure::Unavailable,
            );
        }

        /// The split must not have moved the oversize case, which is the
        /// one append failure a client can fix itself.
        #[test]
        fn an_oversize_batch_is_still_its_own_outcome() {
            assert_eq!(
                IngestFailure::classify(&ReceiveError::WalAppend(AppendError::TooLarge {
                    len: 32 * 1024 * 1024,
                    limit: 16 * 1024 * 1024,
                })),
                IngestFailure::TooLarge,
            );
        }

        /// RFC 0065 §6.3: a group that did not commit the export, or a
        /// node that does not lead it, gives the retryable rejection.
        #[test]
        fn a_replication_failure_is_retryable() {
            use crate::replicated::write::WriteError;
            for error in [
                WriteError::NotEnoughReplicas { group: 3 },
                WriteError::NotLeader {
                    group: 3,
                    leader: Some("receiver-1".to_owned()),
                },
                WriteError::NotHosted { group: 3 },
            ] {
                assert_eq!(
                    IngestFailure::classify(&ReceiveError::Replication(error)),
                    IngestFailure::Unavailable,
                );
            }
        }

        /// RFC 0061 §3.2: an export too large for one WAL frame's entry is
        /// the permanent size rejection, as it is on the local WAL.
        #[test]
        fn a_replicated_entry_over_the_frame_is_too_large() {
            use crate::replicated::write::WriteError;
            let error = WriteError::TooLarge {
                len: ourios_wal::ReplicatedRecord::MAX_ENTRY_PAYLOAD_BYTES + 1,
                limit: ourios_wal::ReplicatedRecord::MAX_ENTRY_PAYLOAD_BYTES,
            };
            assert_eq!(
                IngestFailure::classify(&ReceiveError::Replication(error)),
                IngestFailure::TooLarge,
            );
        }
    }

    /// RFC 0026 §3.2 (relocated here by RFC 0051): `check_binding`
    /// allows a tenant inside the binding's write set and rejects one
    /// outside it, preserving the token's audit label and the selected
    /// tenant in the error.
    #[test]
    fn check_binding_allows_inside_and_denies_outside_the_write_set() {
        let store = ourios_core::auth::build_token_store(Some(&[ourios_core::auth::TokenSpec {
            name: Some("edge".to_string()),
            token: Some("tok-edge".to_string()),
            tenants: vec!["acme".to_string()],
        }]))
        .expect("valid spec")
        .expect("enabled store");
        let binding = ourios_serving::authenticate_bearer(Some(&store), Some("Bearer tok-edge"))
            .expect("known token")
            .expect("bound");

        let acme = ourios_core::tenant::TenantId::new("acme");
        assert!(check_binding(&acme, &binding).is_ok());

        let globex = ourios_core::tenant::TenantId::new("globex");
        match check_binding(&globex, &binding) {
            Err(ReceiveError::TenantDenied { token_name, tenant }) => {
                assert_eq!(token_name, "edge", "audit label preserved");
                assert_eq!(tenant, globex, "selected tenant preserved");
            }
            other => panic!("expected TenantDenied, got {other:?}"),
        }
    }
}
