//! The tenant divergence detector (RFC 0045 §3.4).
//!
//! For each configured `watch` key that is not part of the derivation
//! rule, the detector remembers the first value seen per (tenant, key)
//! and announces — warning + `ourios.receiver.tenant.divergences` — when
//! a later `ResourceLogs` group for the same tenant carries a different
//! value. That is the shape of the misconfiguration the RFC exists to
//! surface: two clusters merging into one tenant under a single-key
//! rule. The detector observes, it never rejects.
//!
//! State is bounded (`watch_capacity` entries, first-come admission, one
//! saturation warning per process), values are bounded (128 bytes,
//! UTF-8-safe truncation), and the warning is rate-limited per
//! (tenant, key). Everything resets on restart by design.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use opentelemetry::metrics::Counter;
use opentelemetry::{KeyValue as OtelKeyValue, global};
use opentelemetry_proto::tonic::common::v1::KeyValue;
use opentelemetry_proto::tonic::common::v1::any_value::Value;
use ourios_core::tenant::TenantId;
use ourios_semconv as semconv;

use super::tenant::TenantDerivation;

/// Longest value the detector remembers or logs, in bytes (RFC 0045
/// §3.4 *Value representation*).
pub const MAX_VALUE_BYTES: usize = 128;

/// Minimum interval between two warnings for the same (tenant, key).
pub const WARN_INTERVAL: Duration = Duration::from_secs(60);

struct Entry {
    /// Exact identity of the first value: digest + byte length. Comparison
    /// never uses the preview, so values sharing a 128-byte prefix are
    /// still told apart.
    first_digest: u64,
    first_len: usize,
    /// The bounded rendering of the first value, for the warning only.
    first_preview: String,
    last_warned: Option<Instant>,
}

fn digest(value: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

/// The detector: build one per pipeline from the resolved
/// [`TenantDerivation`]; call [`observe`](Self::observe) once per
/// derived `ResourceLogs` group.
pub struct DivergenceWatch {
    keys: Vec<String>,
    capacity: usize,
    state: Mutex<HashMap<(TenantId, String), Entry>>,
    saturated: AtomicBool,
    divergences: Counter<u64>,
}

impl std::fmt::Debug for DivergenceWatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DivergenceWatch")
            .field("keys", &self.keys)
            .field("capacity", &self.capacity)
            .finish_non_exhaustive()
    }
}

impl DivergenceWatch {
    /// The detector for `derivation`, watching every `watch` key the rule
    /// does not already join on. `None` when nothing is left to watch.
    #[must_use]
    pub fn from_derivation(derivation: &TenantDerivation) -> Option<Self> {
        let keys: Vec<String> = derivation
            .watch
            .iter()
            .filter(|key| !derivation.rule.contains(key))
            .cloned()
            .collect();
        if keys.is_empty() {
            return None;
        }
        Some(Self::new(keys, derivation.watch_capacity))
    }

    fn new(keys: Vec<String>, capacity: usize) -> Self {
        let divergences = global::meter("ourios.receiver")
            .u64_counter(semconv::OURIOS_RECEIVER_TENANT_DIVERGENCES)
            .build();
        Self {
            keys,
            capacity,
            state: Mutex::new(HashMap::new()),
            saturated: AtomicBool::new(false),
            divergences,
        }
    }

    /// The watched keys (the configured set minus the rule's keys).
    #[must_use]
    pub fn keys(&self) -> &[String] {
        &self.keys
    }

    /// Observe one derived group. A key the resource lacks (or carries as
    /// a non-string or empty string) is skipped for that group.
    pub fn observe(&self, tenant: &TenantId, resource_attributes: &[KeyValue]) {
        for key in &self.keys {
            let Some(value) = string_attribute(resource_attributes, key) else {
                continue;
            };
            self.observe_one(tenant, key, value);
        }
    }

    fn observe_one(&self, tenant: &TenantId, key: &str, value: &str) {
        // Decide under the lock, emit outside it: telemetry can block on a
        // subscriber or exporter and must not stall unrelated ingest.
        let Some(outcome) = self.classify(tenant, key, value) else {
            return;
        };
        match outcome {
            Outcome::Saturated => tracing::warn!(
                name: semconv::EVENT_OURIOS_RECEIVER_TENANT_WATCH_SATURATED,
                "tenant divergence watch is full ({} entries); further (tenant, key) pairs \
                 are not watched — raise receiver.tenant.watch_capacity if this matters",
                self.capacity,
            ),
            Outcome::Diverged {
                first_preview,
                warn,
            } => {
                self.divergences.add(
                    1,
                    &[OtelKeyValue::new(
                        semconv::OURIOS_TENANT_WATCH_KEY,
                        key.to_owned(),
                    )],
                );
                if warn {
                    tracing::event!(
                        name: semconv::EVENT_OURIOS_RECEIVER_TENANT_DIVERGENCE,
                        tracing::Level::WARN,
                        ourios.tenant = tenant.as_str(),
                        ourios.tenant.watch.key = key,
                        ourios.tenant.watch.first_value = first_preview.as_str(),
                        ourios.tenant.watch.value = bound(value).as_ref(),
                        "tenant spans more than one value of a watched resource attribute — if \
                         these are different producers, add the key to receiver.tenant.rule \
                         (RFC 0045 §3.4)"
                    );
                }
            }
        }
    }

    /// The locked half of [`observe_one`](Self::observe_one): admit or
    /// compare, and say what (if anything) to emit once the lock is gone.
    fn classify(&self, tenant: &TenantId, key: &str, value: &str) -> Option<Outcome> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let slot = (tenant.clone(), key.to_owned());
        let Some(entry) = state.get_mut(&slot) else {
            if state.len() >= self.capacity {
                return (!self.saturated.swap(true, Ordering::Relaxed))
                    .then_some(Outcome::Saturated);
            }
            state.insert(
                slot,
                Entry {
                    first_digest: digest(value),
                    first_len: value.len(),
                    first_preview: bound(value).into_owned(),
                    last_warned: None,
                },
            );
            return None;
        };
        if entry.first_len == value.len() && entry.first_digest == digest(value) {
            return None;
        }
        let now = Instant::now();
        let warn = entry
            .last_warned
            .is_none_or(|last| now.duration_since(last) >= WARN_INTERVAL);
        if warn {
            entry.last_warned = Some(now);
        }
        Some(Outcome::Diverged {
            first_preview: entry.first_preview.clone(),
            warn,
        })
    }
}

enum Outcome {
    Saturated,
    Diverged { first_preview: String, warn: bool },
}

fn string_attribute<'a>(attributes: &'a [KeyValue], key: &str) -> Option<&'a str> {
    attributes
        .iter()
        .find(|kv| kv.key == key)
        .and_then(|kv| kv.value.as_ref())
        .and_then(|value| match value.value.as_ref() {
            Some(Value::StringValue(s)) if !s.is_empty() => Some(s.as_str()),
            _ => None,
        })
}

/// `value` bounded to [`MAX_VALUE_BYTES`] *including* the trailing `…`
/// that marks a truncation (cut at a UTF-8 boundary), or borrowed unchanged
/// when it already fits.
fn bound(value: &str) -> std::borrow::Cow<'_, str> {
    if value.len() <= MAX_VALUE_BYTES {
        return std::borrow::Cow::Borrowed(value);
    }
    let mut end = MAX_VALUE_BYTES - '…'.len_utf8();
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    std::borrow::Cow::Owned(format!("{}…", &value[..end]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::receiver::tenant::TenantRule;
    use opentelemetry_proto::tonic::common::v1::AnyValue;

    fn attr(key: &str, value: &str) -> KeyValue {
        KeyValue {
            key: key.to_owned(),
            value: Some(AnyValue {
                value: Some(Value::StringValue(value.to_owned())),
            }),
            ..Default::default()
        }
    }

    fn watch(capacity: usize) -> DivergenceWatch {
        DivergenceWatch::from_derivation(&TenantDerivation {
            rule: TenantRule::service_name(),
            watch: vec!["k8s.cluster.name".to_owned()],
            watch_capacity: capacity,
        })
        .expect("one key to watch")
    }

    fn entries(watch: &DivergenceWatch) -> usize {
        watch.state.lock().expect("lock").len()
    }

    // The tracing field literals above must be the registry names —
    // `tracing` needs literals, the registry needs them registered.
    #[test]
    fn warning_field_names_are_the_registered_attributes() {
        assert_eq!(semconv::OURIOS_TENANT, "ourios.tenant");
        assert_eq!(semconv::OURIOS_TENANT_WATCH_KEY, "ourios.tenant.watch.key");
        assert_eq!(
            semconv::OURIOS_TENANT_WATCH_FIRST_VALUE,
            "ourios.tenant.watch.first_value"
        );
        assert_eq!(
            semconv::OURIOS_TENANT_WATCH_VALUE,
            "ourios.tenant.watch.value"
        );
    }

    // §3.1 — a watch key that is also a rule key is not watched.
    #[test]
    fn keys_in_the_rule_are_not_watched() {
        let derivation = TenantDerivation {
            rule: TenantRule::from_keys(["k8s.cluster.name", "service.name"]).expect("rule"),
            watch: vec!["k8s.cluster.name".to_owned()],
            watch_capacity: 10,
        };
        assert!(DivergenceWatch::from_derivation(&derivation).is_none());
        let derivation = TenantDerivation {
            watch: vec!["k8s.cluster.name".to_owned(), "cloud.region".to_owned()],
            ..derivation
        };
        assert_eq!(
            DivergenceWatch::from_derivation(&derivation)
                .expect("cloud.region remains")
                .keys(),
            ["cloud.region"]
        );
    }

    // RFC0045.7 — first value remembered; the same value is not a
    // divergence; a group lacking the key (or non-string / empty) is skipped.
    #[test]
    fn remembers_first_value_and_skips_absent_keys() {
        let w = watch(10);
        let tenant = TenantId::new("fluxcd");
        w.observe(&tenant, &[attr("k8s.cluster.name", "cluster1")]);
        w.observe(&tenant, &[attr("k8s.cluster.name", "cluster1")]);
        w.observe(&tenant, &[attr("service.name", "fluxcd")]);
        w.observe(&tenant, &[attr("k8s.cluster.name", "")]);
        w.observe(
            &tenant,
            &[KeyValue {
                key: "k8s.cluster.name".to_owned(),
                value: Some(AnyValue {
                    value: Some(Value::IntValue(3)),
                }),
                ..Default::default()
            }],
        );
        assert_eq!(entries(&w), 1);
        let state = w.state.lock().expect("lock");
        let entry = state
            .get(&(tenant, "k8s.cluster.name".to_owned()))
            .expect("entry");
        assert_eq!(entry.first_preview, "cluster1");
        assert!(entry.last_warned.is_none(), "no divergence, no warning");
    }

    // RFC0045.7 — a different value is a divergence (warned once per
    // interval); the first value never changes.
    #[test]
    fn divergent_value_warns_once_per_interval() {
        let w = watch(10);
        let tenant = TenantId::new("fluxcd");
        w.observe(&tenant, &[attr("k8s.cluster.name", "cluster1")]);
        w.observe(&tenant, &[attr("k8s.cluster.name", "cluster2")]);
        let first_warn = {
            let state = w.state.lock().expect("lock");
            let entry = state
                .get(&(tenant.clone(), "k8s.cluster.name".to_owned()))
                .expect("entry");
            assert_eq!(entry.first_preview, "cluster1");
            entry.last_warned.expect("warned")
        };
        w.observe(&tenant, &[attr("k8s.cluster.name", "cluster3")]);
        let state = w.state.lock().expect("lock");
        let entry = state
            .get(&(tenant, "k8s.cluster.name".to_owned()))
            .expect("entry");
        assert_eq!(entry.last_warned, Some(first_warn), "rate-limited");
    }

    // RFC0045.9 — capacity: first-come admission, saturation announced once.
    #[test]
    fn capacity_bounds_admission() {
        let w = watch(1);
        w.observe(&TenantId::new("a"), &[attr("k8s.cluster.name", "c1")]);
        w.observe(&TenantId::new("b"), &[attr("k8s.cluster.name", "c1")]);
        assert_eq!(entries(&w), 1);
        assert!(w.saturated.load(Ordering::Relaxed));
        // The admitted tenant is still watched.
        w.observe(&TenantId::new("a"), &[attr("k8s.cluster.name", "c2")]);
        let state = w.state.lock().expect("lock");
        assert!(
            state[&(TenantId::new("a"), "k8s.cluster.name".to_owned())]
                .last_warned
                .is_some()
        );
    }

    // RFC0045.7 — two values sharing their first 128 bytes still diverge:
    // comparison is digest + length, the preview is display only.
    #[test]
    fn shared_prefix_values_still_diverge() {
        let w = watch(10);
        let tenant = TenantId::new("t");
        let prefix = "p".repeat(MAX_VALUE_BYTES);
        w.observe(
            &tenant,
            &[attr("k8s.cluster.name", &format!("{prefix}-one"))],
        );
        w.observe(
            &tenant,
            &[attr("k8s.cluster.name", &format!("{prefix}-two"))],
        );
        let state = w.state.lock().expect("lock");
        let entry = &state[&(tenant, "k8s.cluster.name".to_owned())];
        assert!(
            entry.last_warned.is_some(),
            "divergence detected past the preview bound"
        );
        assert_eq!(
            entry.first_preview,
            format!("{}…", "p".repeat(MAX_VALUE_BYTES - '…'.len_utf8()))
        );
    }

    // RFC0045.7 — values are bounded at a UTF-8 boundary with a trailing `…`.
    #[test]
    fn values_are_bounded_at_a_char_boundary() {
        let short = "x".repeat(MAX_VALUE_BYTES);
        assert_eq!(bound(&short), short);
        // 'é' is two bytes and straddles the cut point (125 bytes into a
        // 128-byte budget with a 3-byte ellipsis): the cut backs off to the
        // char boundary before it.
        let long = format!("{}é{}", "x".repeat(MAX_VALUE_BYTES - 4), "tail");
        let bounded = bound(&long);
        assert_eq!(bounded, format!("{}…", "x".repeat(MAX_VALUE_BYTES - 4)));
        assert!(bounded.len() <= MAX_VALUE_BYTES);
        let ascii = "y".repeat(MAX_VALUE_BYTES + 10);
        assert_eq!(bound(&ascii).len(), MAX_VALUE_BYTES);
    }
}
