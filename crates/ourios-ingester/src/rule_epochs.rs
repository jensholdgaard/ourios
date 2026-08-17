//! The tenant rule-epoch log (RFC 0045 §3.3): which [`TenantRule`] each
//! WAL frame was acknowledged under, so startup replay derives a frame's
//! tenant exactly as ingest did even after the operator changed the rule.
//!
//! A sidecar file in the WAL root (`tenant_rule_epochs.json`), never a
//! WAL frame kind: an ordered list of `{rule, after}` entries meaning
//! "frames with offset > `after` derive under `rule`" (`after: null` =
//! from the beginning). An absent file is the single implicit epoch
//! `{[service.name], null}` — every pre-RFC WAL — so upgrading needs no
//! migration. A file that exists but does not parse aborts startup, the
//! same class as a corrupt segment header.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

use ourios_wal::WalOffset;
use serde_json::{Value, json};

use crate::receiver::tenant::TenantRule;

/// The sidecar's file name inside the WAL root.
pub const FILE_NAME: &str = "tenant_rule_epochs.json";

/// One epoch: `rule` applies to frames strictly after `after`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleEpoch {
    pub rule: TenantRule,
    pub after: Option<WalOffset>,
}

/// The loaded epoch log, in append order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleEpochs {
    path: PathBuf,
    epochs: Vec<RuleEpoch>,
}

impl RuleEpochs {
    /// Load `<wal_root>/tenant_rule_epochs.json`, or the implicit
    /// `[service.name]` epoch when the file is absent.
    ///
    /// # Errors
    ///
    /// [`RuleEpochsError`] on an I/O failure other than not-found, or a
    /// file that is not the documented shape.
    pub fn load(wal_root: &Path) -> Result<Self, RuleEpochsError> {
        let path = wal_root.join(FILE_NAME);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self {
                    path,
                    epochs: vec![RuleEpoch {
                        rule: TenantRule::service_name(),
                        after: None,
                    }],
                });
            }
            Err(source) => {
                return Err(RuleEpochsError::Io {
                    op: "read(tenant rule epochs)",
                    path,
                    source,
                });
            }
        };
        let epochs = parse(&bytes).map_err(|detail| RuleEpochsError::Malformed {
            path: path.clone(),
            detail,
        })?;
        Ok(Self { path, epochs })
    }

    /// The epochs, oldest first.
    #[must_use]
    pub fn epochs(&self) -> &[RuleEpoch] {
        &self.epochs
    }

    /// The rule the newest epoch derives under.
    #[must_use]
    pub fn current(&self) -> &TenantRule {
        // `load` and `advance` keep the list non-empty.
        self.epochs
            .last()
            .map_or_else(|| unreachable!("epoch log is never empty"), |e| &e.rule)
    }

    /// The rule a frame at `offset` was acknowledged under: the newest
    /// epoch whose `after` lies strictly below `offset`.
    #[must_use]
    pub fn rule_for(&self, offset: WalOffset) -> &TenantRule {
        self.epochs
            .iter()
            .rev()
            .find(|epoch| epoch.after.is_none_or(|after| offset > after))
            .map_or_else(|| self.current(), |epoch| &epoch.rule)
    }

    /// Make `rule` the current epoch for frames after `after` (the highest
    /// offset replay delivered), persisting the log if the rule differs
    /// from the newest epoch's. Returns whether an epoch was appended.
    ///
    /// # Errors
    ///
    /// [`RuleEpochsError::Io`] if the sidecar cannot be written durably.
    pub fn advance(
        &mut self,
        rule: &TenantRule,
        after: Option<WalOffset>,
    ) -> Result<bool, RuleEpochsError> {
        if self.current() == rule {
            return Ok(false);
        }
        self.epochs.push(RuleEpoch {
            rule: rule.clone(),
            after,
        });
        self.persist()?;
        Ok(true)
    }

    fn persist(&self) -> Result<(), RuleEpochsError> {
        let io = |op: &'static str, path: &Path| {
            let path = path.to_path_buf();
            move |source| RuleEpochsError::Io { op, path, source }
        };
        let bytes = serde_json::to_vec_pretty(&render(&self.epochs)).map_err(|e| {
            RuleEpochsError::Malformed {
                path: self.path.clone(),
                detail: e.to_string(),
            }
        })?;
        let tmp = self.path.with_extension("json.tmp");
        let mut file = File::create(&tmp).map_err(io("create(tenant rule epochs tmp)", &tmp))?;
        file.write_all(&bytes)
            .map_err(io("write(tenant rule epochs tmp)", &tmp))?;
        file.sync_all()
            .map_err(io("fsync(tenant rule epochs tmp)", &tmp))?;
        std::fs::rename(&tmp, &self.path)
            .map_err(io("rename(tenant rule epochs tmp -> live)", &self.path))?;
        if let Some(dir) = self.path.parent() {
            File::open(dir)
                .and_then(|d| d.sync_all())
                .map_err(io("fsync(wal root after tenant rule epochs)", dir))?;
        }
        Ok(())
    }
}

fn render(epochs: &[RuleEpoch]) -> Value {
    json!({
        "epochs": epochs
            .iter()
            .map(|epoch| {
                json!({
                    "rule": epoch.rule.keys(),
                    "after": epoch.after.map(|o| json!({
                        "segment": o.segment.to_string(),
                        "byte": o.byte,
                    })),
                })
            })
            .collect::<Vec<_>>(),
    })
}

fn parse(bytes: &[u8]) -> Result<Vec<RuleEpoch>, String> {
    let root: Value = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    let entries = root
        .get("epochs")
        .and_then(Value::as_array)
        .ok_or("missing `epochs` array")?;
    if entries.is_empty() {
        return Err("`epochs` is empty".to_owned());
    }
    let mut epochs = Vec::with_capacity(entries.len());
    let mut previous: Option<WalOffset> = None;
    for (index, entry) in entries.iter().enumerate() {
        let keys = entry
            .get("rule")
            .and_then(Value::as_array)
            .ok_or_else(|| format!("epochs[{index}].rule is not an array"))?
            .iter()
            .map(|k| {
                k.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| format!("epochs[{index}].rule holds a non-string key"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let rule = TenantRule::from_keys(keys).map_err(|e| format!("epochs[{index}].rule: {e}"))?;
        let after = match entry.get("after") {
            None | Some(Value::Null) => None,
            Some(after) => {
                let segment = after
                    .get("segment")
                    .and_then(Value::as_str)
                    .and_then(|s| s.parse::<uuid::Uuid>().ok())
                    .ok_or_else(|| format!("epochs[{index}].after.segment is not a UUID"))?;
                let byte = after
                    .get("byte")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| format!("epochs[{index}].after.byte is not a u64"))?;
                Some(WalOffset { segment, byte })
            }
        };
        if let (Some(prev), Some(cur)) = (previous, after)
            && cur < prev
        {
            return Err(format!("epochs[{index}].after precedes the previous epoch"));
        }
        previous = after.or(previous);
        epochs.push(RuleEpoch { rule, after });
    }
    Ok(epochs)
}

/// Failure loading or persisting the epoch log.
#[derive(Debug)]
#[non_exhaustive]
pub enum RuleEpochsError {
    Io {
        op: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
    /// The file exists but is not the documented shape — corruption class,
    /// surfaced loudly rather than guessed at.
    Malformed { path: PathBuf, detail: String },
}

impl std::fmt::Display for RuleEpochsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { op, path, source } => {
                write!(f, "tenant rule epochs {op} {}: {source}", path.display())
            }
            Self::Malformed { path, detail } => {
                write!(
                    f,
                    "tenant rule epochs {} is malformed: {detail}",
                    path.display()
                )
            }
        }
    }
}

impl std::error::Error for RuleEpochsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Malformed { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offset(byte: u64) -> WalOffset {
        WalOffset {
            segment: uuid::Uuid::now_v7(),
            byte,
        }
    }

    // RFC0045.10 — an absent log is the implicit [service.name] epoch.
    #[test]
    fn absent_log_is_the_service_name_epoch() {
        let dir = tempfile::tempdir().expect("tempdir");
        let epochs = RuleEpochs::load(dir.path()).expect("loads");
        assert_eq!(epochs.current(), &TenantRule::service_name());
        assert_eq!(epochs.rule_for(offset(0)), &TenantRule::service_name());
        assert!(!dir.path().join(FILE_NAME).exists(), "load never writes");
    }

    // RFC0045.10 — advancing persists; reload sees the same log; frames at or
    // below the boundary keep the old rule, frames above take the new one.
    #[test]
    fn advance_persists_and_lookup_is_by_offset() {
        let dir = tempfile::tempdir().expect("tempdir");
        let composite = TenantRule::from_keys(["k8s.cluster.name", "service.name"]).expect("valid");
        let boundary = offset(100);

        let mut epochs = RuleEpochs::load(dir.path()).expect("loads");
        assert!(
            !epochs
                .advance(&TenantRule::service_name(), Some(boundary))
                .expect("no-op")
        );
        assert!(
            !dir.path().join(FILE_NAME).exists(),
            "unchanged rule never writes"
        );
        assert!(epochs.advance(&composite, Some(boundary)).expect("appends"));

        let reloaded = RuleEpochs::load(dir.path()).expect("reloads");
        assert_eq!(reloaded.epochs(), epochs.epochs());
        assert_eq!(reloaded.current(), &composite);
        let earlier = WalOffset {
            segment: boundary.segment,
            byte: 100,
        };
        let later = WalOffset {
            segment: boundary.segment,
            byte: 101,
        };
        assert_eq!(reloaded.rule_for(earlier), &TenantRule::service_name());
        assert_eq!(reloaded.rule_for(later), &composite);
        assert_eq!(reloaded.rule_for(offset(0)), &composite, "a newer segment");
    }

    // RFC0045.10 — an unparseable log aborts loudly, naming the file.
    #[test]
    fn malformed_log_is_an_error_naming_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(FILE_NAME), b"{\"epochs\": []}").expect("write");
        let err = RuleEpochs::load(dir.path()).unwrap_err();
        assert!(matches!(err, RuleEpochsError::Malformed { .. }), "{err:?}");
        assert!(err.to_string().contains(FILE_NAME));

        std::fs::write(dir.path().join(FILE_NAME), b"not json").expect("write");
        assert!(matches!(
            RuleEpochs::load(dir.path()).unwrap_err(),
            RuleEpochsError::Malformed { .. }
        ));

        std::fs::write(
            dir.path().join(FILE_NAME),
            b"{\"epochs\": [{\"rule\": [\"a\", \"a\"], \"after\": null}]}",
        )
        .expect("write");
        assert!(matches!(
            RuleEpochs::load(dir.path()).unwrap_err(),
            RuleEpochsError::Malformed { .. }
        ));

        // Boundaries that go backwards are rejected; an equal boundary is
        // append order and accepted.
        let later = offset(5);
        let earlier = WalOffset {
            segment: later.segment,
            byte: 4,
        };
        let entry = |keys: &str, o: WalOffset| {
            format!(
                "{{\"rule\": [{keys}], \"after\": {{\"segment\": \"{}\", \"byte\": {}}}}}",
                o.segment, o.byte
            )
        };
        std::fs::write(
            dir.path().join(FILE_NAME),
            format!(
                "{{\"epochs\": [{}, {}]}}",
                entry("\"a\"", later),
                entry("\"b\"", earlier)
            ),
        )
        .expect("write");
        assert!(matches!(
            RuleEpochs::load(dir.path()).unwrap_err(),
            RuleEpochsError::Malformed { .. }
        ));
        std::fs::write(
            dir.path().join(FILE_NAME),
            format!(
                "{{\"epochs\": [{}, {}]}}",
                entry("\"a\"", later),
                entry("\"b\"", later)
            ),
        )
        .expect("write");
        let epochs = RuleEpochs::load(dir.path()).expect("equal boundary is append order");
        assert_eq!(epochs.current().keys(), ["b"]);
    }
}
