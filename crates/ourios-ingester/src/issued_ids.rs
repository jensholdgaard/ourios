//! The highest `template_id` the durable audit stream carries, across
//! every tenant (issue #898).
//!
//! The id allocator is cluster-wide (RFC 0001 §6.1) and lives only in
//! memory; a restored snapshot carries the ids it folded, and replay
//! re-mints the rest. When a snapshot is discarded, or a tenant's
//! reclaimed frames have no restored snapshot over them, the rebuilt
//! allocator can start below ids that published rows already carry, and
//! the next fresh template would take one of them — a silent merge
//! (`CLAUDE.md` §3.1). Object storage is the truth (§3.6): every
//! published row's template has its event in the audit stream before the
//! row is durable, so the stream's highest id bounds every id a row can
//! carry.

use ourios_core::audit::{AuditEvent, AuditPayload};
use ourios_parquet::{AuditReader, AuditReaderError, Store, StoreError};

/// The audit stream's top-level prefix in the store (RFC 0005 §3.4).
const AUDIT_PREFIX: &str = "audit";

/// Why the audit stream's highest template id could not be read. The
/// sources are boxed: both are large, and this rides in startup
/// recovery's error.
#[derive(Debug)]
pub enum IssuedIdsError {
    /// Listing the audit prefix failed.
    List(Box<StoreError>),
    /// Fetching one audit object failed.
    Fetch {
        key: String,
        source: Box<StoreError>,
    },
    /// One audit object does not decode.
    Decode {
        key: String,
        source: Box<AuditReaderError>,
    },
}

impl std::fmt::Display for IssuedIdsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::List(e) => write!(f, "list the audit stream: {e}"),
            Self::Fetch { key, source } => write!(f, "fetch audit file {key}: {source}"),
            Self::Decode { key, source } => write!(f, "decode audit file {key}: {source}"),
        }
    }
}

impl std::error::Error for IssuedIdsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::List(e) | Self::Fetch { source: e, .. } => Some(e.as_ref()),
            Self::Decode { source, .. } => Some(source.as_ref()),
        }
    }
}

/// The highest `template_id` any template event in `store`'s audit stream
/// binds, over every tenant, or `None` when the stream holds none. One
/// audit file is held at a time.
///
/// # Errors
///
/// [`IssuedIdsError`] when the stream cannot be listed or an audit file
/// cannot be fetched or decoded: a floor taken over a partial read could
/// sit below an id a row carries.
pub fn highest_issued_template_id(store: &Store) -> Result<Option<u64>, IssuedIdsError> {
    let keys = store
        .list_blocking(Some(AUDIT_PREFIX))
        .map_err(|e| IssuedIdsError::List(Box::new(e)))?;
    let mut highest = None;
    for key in keys.iter().filter(|key| key.ends_with(".parquet")) {
        let file_highest = highest_in(&read_events(store, key)?);
        highest = highest.max(file_highest);
    }
    Ok(highest)
}

fn read_events(store: &Store, key: &str) -> Result<Vec<AuditEvent>, IssuedIdsError> {
    let bytes = store
        .get_blocking(key)
        .map_err(|source| IssuedIdsError::Fetch {
            key: key.to_owned(),
            source: Box::new(source),
        })?;
    AuditReader::open_bytes(bytes::Bytes::from(bytes))
        .and_then(AuditReader::read_all)
        .map_err(|source| IssuedIdsError::Decode {
            key: key.to_owned(),
            source: Box::new(source),
        })
}

fn highest_in(events: &[AuditEvent]) -> Option<u64> {
    events
        .iter()
        .filter_map(|event| match event.payload {
            AuditPayload::Template { template_id, .. } => Some(template_id),
            _ => None,
        })
        .max()
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use ourios_core::audit::{AuditPayload, TemplateChange};
    use ourios_core::tenant::TenantId;
    use ourios_parquet::{AuditWriter, derive_audit_partition};

    use super::*;

    fn created(tenant: &str, template_id: u64) -> AuditEvent {
        AuditEvent {
            tenant_id: TenantId::new(tenant),
            timestamp: SystemTime::UNIX_EPOCH,
            payload: AuditPayload::Template {
                template_id,
                triggering_line_hash: [0; 16],
                triggering_line_sample: None,
                change: TemplateChange::Created {
                    new_template: format!("line {template_id}"),
                },
            },
        }
    }

    fn publish(store: &Store, events: &[AuditEvent]) {
        let partition = derive_audit_partition(&events[0]).expect("partition");
        let mut writer = AuditWriter::open_in(store, partition).expect("writer");
        writer.append_events(events).expect("append");
        writer.close().expect("close");
    }

    #[test]
    fn an_empty_stream_issues_nothing() {
        let tmp = tempfile::TempDir::new().expect("temp");
        let store = Store::local(tmp.path()).expect("store");
        assert_eq!(highest_issued_template_id(&store).expect("read"), None);
    }

    #[test]
    fn the_highest_id_spans_every_tenant_and_file() {
        let tmp = tempfile::TempDir::new().expect("temp");
        let store = Store::local(tmp.path()).expect("store");
        publish(&store, &[created("checkout", 3), created("checkout", 9)]);
        publish(&store, &[created("search", 12)]);
        publish(&store, &[created("checkout", 4)]);
        assert_eq!(highest_issued_template_id(&store).expect("read"), Some(12));
    }

    #[test]
    fn an_undecodable_audit_file_fails_the_read() {
        let tmp = tempfile::TempDir::new().expect("temp");
        let store = Store::local(tmp.path()).expect("store");
        publish(&store, &[created("checkout", 3)]);
        let dir = tmp.path().join("audit/tenant_id=checkout");
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(dir.join("torn.parquet"), b"not parquet").expect("torn file");
        let err = highest_issued_template_id(&store).expect_err("a partial read is no floor");
        assert!(matches!(err, IssuedIdsError::Decode { .. }), "{err}");
    }
}
