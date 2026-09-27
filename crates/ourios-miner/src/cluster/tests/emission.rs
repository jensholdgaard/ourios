use super::*;

// ---------- record emission (RFC §6.1 / §6.6 scaffolding) ----------

#[test]
fn body_none_emits_absent_record_with_no_template() {
    let records = SharedRecordSink::new();
    let mut cluster =
        MinerCluster::new(MinerConfig::default()).with_record_sink(Box::new(records.clone()));
    let t = TenantId::new("tenant-x");

    let r = OtlpLogRecord {
        tenant_id: t.clone(),
        body: None,
        ..Default::default()
    };
    let id = cluster.ingest(&r);

    assert_eq!(id, NO_TEMPLATE);
    let emitted = records.drain();
    assert_eq!(emitted.len(), 1);
    let rec = &emitted[0];
    assert_eq!(rec.tenant_id, t);
    assert_eq!(rec.template_id, NO_TEMPLATE);
    assert_eq!(rec.body_kind, BodyKind::Absent);
    assert!(
        !rec.lossy_flag,
        "absence is not loss (RFC 0025 §3.1) — reconstruction renders nothing, exactly"
    );
    assert!(rec.separators.is_empty());
    assert!(rec.params.is_empty());
    assert!(rec.body.is_none());
}

#[test]
fn clean_fresh_leaf_emits_record_with_separators_and_no_body() {
    let (mut cluster, _audit, records) = cluster_with_observable_sinks();
    let t = TenantId::new("tenant-x");

    let _ = cluster.ingest(&string_record(&t, "user 42 logged in"));

    let emitted = records.drain();
    assert_eq!(emitted.len(), 1);
    let rec = &emitted[0];
    assert_eq!(rec.body_kind, BodyKind::String);
    assert_ne!(rec.template_id, NO_TEMPLATE);
    assert_eq!(rec.template_version, 1);
    // tokenize("user 42 logged in") yields 4 tokens → 5
    // separators per the §6.6 capture invariant.
    assert_eq!(rec.separators.len(), 5);
    // Clean attaches do not retain body and are not lossy.
    assert!(rec.body.is_none());
    assert!(!rec.lossy_flag);
    // sim_seq against a fresh leaf is 1.0 by definition;
    // confidence = sim / threshold = 1.0 / 0.7 ≈ 1.428, but
    // the cluster reports the sentinel 1.0 for clean attaches.
    assert!((rec.confidence - 1.0).abs() < f32::EPSILON);
}

#[test]
fn clean_reuse_emits_record_at_same_template_id_and_version() {
    let (mut cluster, _audit, records) = cluster_with_observable_sinks();
    let t = TenantId::new("tenant-x");

    let id1 = cluster.ingest(&string_record(&t, "user 42 logged in"));
    let id2 = cluster.ingest(&string_record(&t, "user 17 logged in"));

    assert_eq!(id1, id2, "reuse same template");
    let emitted = records.drain();
    assert_eq!(emitted.len(), 2);
    assert_eq!(emitted[0].template_id, id1);
    assert_eq!(emitted[1].template_id, id1);
    assert_eq!(emitted[0].template_version, 1);
    assert_eq!(
        emitted[1].template_version, 1,
        "clean reuse must not bump the version",
    );
}

#[test]
fn widening_emits_record_with_bumped_version() {
    let (mut cluster, _audit, records) = cluster_with_observable_sinks();
    let t = TenantId::new("tenant-x");

    let _ = cluster.ingest(&string_record(&t, "user 42 logged in from 10.0.0.1"));
    let _ = cluster.ingest(&string_record(&t, "user 42 logged out from 10.0.0.1"));

    let emitted = records.drain();
    assert_eq!(emitted.len(), 2);
    assert_eq!(emitted[0].template_version, 1, "L1 at fresh-leaf version");
    assert_eq!(
        emitted[1].template_version, 2,
        "L2's widening bumps version on the same template_id",
    );
    assert_eq!(
        emitted[0].template_id, emitted[1].template_id,
        "widening attaches to the same template_id",
    );
}

#[test]
fn lossy_attach_emits_record_with_retained_body_and_lossy_flag_false() {
    let (mut cluster, _audit, records) = cluster_with_observable_sinks();
    let t = TenantId::new("tenant-x");

    // L2 = sim 3/5 = 0.6 ∈ [0.4, 0.7) → lossy zone.
    let _ = cluster.ingest(&string_record(&t, "alpha beta gamma delta epsilon"));
    let l2_raw = "alpha beta gamma rho sigma";
    let _ = cluster.ingest(&string_record(&t, l2_raw));

    let emitted = records.drain();
    assert_eq!(emitted.len(), 2);
    let lossy = &emitted[1];
    assert_eq!(lossy.body.as_deref(), Some(l2_raw));
    // §6.6: the lossy zone retains body but `lossy_flag`
    // stays false — reconstruction is expected to match.
    assert!(!lossy.lossy_flag);
    // confidence = sim / threshold = 0.6 / 0.7.
    let expected_conf = 0.6_f32 / 0.7_f32;
    assert!(
        (lossy.confidence - expected_conf).abs() < 1e-4,
        "expected confidence ≈ {expected_conf}, got {}",
        lossy.confidence,
    );
    // Lossy attach creates a fresh leaf, so version is 1.
    assert_eq!(lossy.template_version, 1);
    assert_ne!(lossy.template_id, NO_TEMPLATE);
}

#[test]
fn parse_failure_zone_emits_record_with_lossy_flag_and_no_template() {
    let (mut cluster, _audit, records) = cluster_with_observable_sinks();
    let t = TenantId::new("tenant-x");

    // sim 2/6 ≈ 0.333 < 0.4 floor → parse-failure zone.
    let _ = cluster.ingest(&string_record(&t, "alpha beta gamma delta epsilon zeta"));
    let l2_raw = "alpha beta phi rho sigma omega";
    let _ = cluster.ingest(&string_record(&t, l2_raw));

    let emitted = records.drain();
    assert_eq!(emitted.len(), 2);
    let pf = &emitted[1];
    assert_eq!(pf.template_id, NO_TEMPLATE);
    assert_eq!(pf.template_version, 0);
    assert!(pf.lossy_flag, "parse-failure records are lossy");
    assert_eq!(pf.body.as_deref(), Some(l2_raw));
    assert!(
        pf.confidence.abs() < f32::EPSILON,
        "parse-failure confidence is the 0.0 sentinel",
    );
}

#[test]
fn empty_input_emits_parse_failure_record() {
    let records = SharedRecordSink::new();
    let mut cluster =
        MinerCluster::new(MinerConfig::default()).with_record_sink(Box::new(records.clone()));
    let t = TenantId::new("tenant-x");

    let _ = cluster.ingest(&string_record(&t, ""));

    let emitted = records.drain();
    assert_eq!(emitted.len(), 1);
    let rec = &emitted[0];
    assert_eq!(rec.template_id, NO_TEMPLATE);
    assert!(rec.lossy_flag);
    assert_eq!(rec.body.as_deref(), Some(""));
    // §6.6 capture invariant on the degenerate case: empty
    // input still has separators.len() == tokens.len() + 1.
    assert_eq!(rec.separators.len(), 1);
}

#[test]
fn structured_body_emits_record_with_structured_kind() {
    let records = SharedRecordSink::new();
    let mut cluster =
        MinerCluster::new(MinerConfig::default()).with_record_sink(Box::new(records.clone()));
    let t = TenantId::new("tenant-x");

    let _ = cluster.ingest(&structured_record(&t, 9, Some("lib.auth")));

    let emitted = records.drain();
    assert_eq!(emitted.len(), 1);
    let rec = &emitted[0];
    assert_eq!(rec.body_kind, BodyKind::Structured);
    assert_ne!(rec.template_id, NO_TEMPLATE);
    assert_eq!(rec.template_version, 1);
    // RFC §6.1: Structured records always carry
    // `lossy_flag = false`. The producer populates `body`
    // with the Ourios-canonical JSON encoding of the
    // structured value (`ingest_structured` →
    // `canonical::encode_any_value`), so `reconstruct()`
    // returns what we stored, satisfying §3.3.
    assert!(rec.separators.is_empty());
    assert!(rec.params.is_empty());
    assert!(
        rec.body.is_some(),
        "structured records must carry the stored body representation"
    );
    assert!(!rec.lossy_flag);
}

#[test]
fn default_sink_drops_records_silently() {
    // `MinerCluster::new` defaults to `NoOpRecordSink`; tests
    // that don't opt into `with_record_sink` simply see no
    // records (the cluster doesn't crash, doesn't allocate,
    // doesn't expose state). Pins the production-safe default.
    let mut cluster = MinerCluster::new(MinerConfig::default());
    let t = TenantId::new("tenant-x");
    let _ = cluster.ingest(&string_record(&t, "user 42 logged in"));
    // No assertion beyond "the call succeeded" — the contract
    // is no public observable side effect.
    assert_eq!(cluster.template_count(&t), 1);
}

/// RFC 0035 §3.1 — `ingest_mined` is `ingest` with the sink emit
/// diverted to the caller: identical template-id assignment, audit
/// stream, and per-tenant state, and the returned record is
/// field-identical to what `ingest` hands the record sink.
#[test]
fn rfc0035_ingest_mined_matches_ingest_except_the_sink_emit() {
    use ourios_core::clock::TestClock;

    let records = SharedRecordSink::new();
    let audit = SharedAuditSink::new();
    let mut via_ingest =
        MinerCluster::with_audit_sink(MinerConfig::default(), Box::new(audit.clone()))
            .with_record_sink(Box::new(records.clone()))
            .with_clock(Box::new(TestClock::epoch()));
    let mined_audit = SharedAuditSink::new();
    let mined_records = SharedRecordSink::new();
    let mut via_mined =
        MinerCluster::with_audit_sink(MinerConfig::default(), Box::new(mined_audit.clone()))
            .with_record_sink(Box::new(mined_records.clone()))
            .with_clock(Box::new(TestClock::epoch()));
    let t = TenantId::new("tenant-x");

    // A mix that exercises fresh-leaf, attach, widening, structured,
    // and parse-failure paths through both entry points.
    let inputs = [
        string_record(&t, "user 42 logged in"),
        string_record(&t, "user 43 logged in"),
        string_record(&t, "user alpha logged in"),
        string_record(&t, ""),
        structured_record(&t, 9, Some("lib.auth")),
    ];
    let mut captured = Vec::new();
    for input in &inputs {
        let id = via_ingest.ingest(input);
        let (mined_id, mined) = via_mined.ingest_mined(input);
        assert_eq!(id, mined_id, "identical template-id assignment");
        captured.push(mined.expect(
            "every ingest emits exactly one record, so every ingest_mined captures exactly one",
        ));
    }

    assert!(
        mined_records.drain().is_empty(),
        "the capture slot diverts the record away from the miner's own sink",
    );
    assert_eq!(
        records.drain(),
        captured,
        "the captured record equals what ingest hands the sink",
    );
    assert_eq!(
        audit.drain(),
        mined_audit.drain(),
        "audit emission stays in the ordered phase, identical on both paths",
    );
    assert_eq!(
        via_ingest.snapshot_state(&t),
        via_mined.snapshot_state(&t),
        "per-tenant state identical across the two entry points",
    );
}

/// An audit sink that panics on its first `Created` event, once —
/// the injectable mid-`ingest` panic (audit runs in the ordered
/// phase, before the record emit).
struct PanicOnceAuditSink {
    fired: bool,
}

impl AuditSink for PanicOnceAuditSink {
    fn emit(&mut self, _event: AuditEvent) {
        if !self.fired {
            self.fired = true;
            panic!("injected audit-sink panic");
        }
    }
}

/// RFC 0035 review F2 — a panic inside `ingest_mined` must leave the
/// capture slot clean: the next call captures normally instead of
/// silently losing its record to a stale slot state.
#[test]
fn rfc0035_f2_capture_slot_is_clean_after_a_panic() {
    let records = SharedRecordSink::new();
    let mut cluster = MinerCluster::with_audit_sink(
        MinerConfig::default(),
        Box::new(PanicOnceAuditSink { fired: false }),
    )
    .with_record_sink(Box::new(records.clone()));
    let t = TenantId::new("tenant-x");

    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        cluster.ingest_mined(&string_record(&t, "user 1 logged in"))
    }));
    assert!(panicked.is_err(), "the injected audit panic propagates");
    // The panic fired before the record emit (audit precedes it), so
    // nothing was captured and nothing is salvaged.
    assert_eq!(cluster.mined_capture_salvages_total(), 0);

    let (_, mined) = cluster.ingest_mined(&string_record(&t, "user 2 logged in"));
    assert!(
        mined.is_some(),
        "the slot was reset across the unwind — the next batch item is \
         captured, not swallowed by a stale Armed/Captured state",
    );
    assert!(
        records.drain().is_empty(),
        "capture still diverts away from the miner's own sink",
    );
}

/// RFC 0035 review F2 — the unwind settle forwards a record captured
/// before the panic to the real sink (and counts it) instead of
/// dropping an acknowledged record until restart replay.
#[test]
fn rfc0035_f2_salvage_forwards_a_captured_record_to_the_sink() {
    let records = SharedRecordSink::new();
    let mut cluster =
        MinerCluster::new(MinerConfig::default()).with_record_sink(Box::new(records.clone()));
    let t = TenantId::new("tenant-x");

    // Stage the state a panic-after-capture leaves behind (the tail
    // of `ingest` past the emit is not injectable from outside).
    let (_, mined) = cluster.ingest_mined(&string_record(&t, "user 1 logged in"));
    cluster.mined_capture = MinedCapture::Captured(Box::new(mined.expect("captured")));

    cluster.salvage_mined_capture();

    assert_eq!(cluster.mined_capture_salvages_total(), 1);
    let salvaged = records.drain();
    assert_eq!(salvaged.len(), 1, "the captured record reached the sink");
    assert!(
        matches!(cluster.mined_capture, MinedCapture::Off),
        "the slot is reset after the salvage",
    );
}
