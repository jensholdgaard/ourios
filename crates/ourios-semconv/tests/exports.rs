//! Locks the generated public name constants to their registry values.
//! A template/registry change that renames or drops a constant breaks
//! this compile/assert, alongside the CI codegen no-diff check.

#[test]
fn metric_and_attribute_names_match_the_registry() {
    // Arrange / Act / Assert — the constants are the source of truth
    // for instrument names; assert a representative metric + attribute.
    assert_eq!(
        ourios_semconv::OURIOS_COMPACTION_SWEEPS,
        "ourios.compaction.sweeps"
    );
    assert_eq!(
        ourios_semconv::OURIOS_STORAGE_PARQUET_FILE_SIZE,
        "ourios.storage.parquet.file.size"
    );
    assert_eq!(ourios_semconv::OURIOS_TENANT, "ourios.tenant");
    assert_eq!(
        ourios_semconv::OURIOS_COMPACTION_RESULT,
        "ourios.compaction.result"
    );
}

#[test]
fn miner_names_match_the_registry() {
    // Arrange / Act / Assert — the RFC 0001 §6.8 miner metric set and
    // its attributes, dotted-`ourios.miner.*` per the audited registry
    // (`.utilization` for fraction-of-total gauges, `duration` not
    // `latency`, no `_total` counter suffix).
    assert_eq!(
        ourios_semconv::OURIOS_MINER_TEMPLATE_COUNT,
        "ourios.miner.template.count"
    );
    assert_eq!(ourios_semconv::OURIOS_MINER_MERGES, "ourios.miner.merges");
    assert_eq!(
        ourios_semconv::OURIOS_MINER_PARSE_FAILURES,
        "ourios.miner.parse_failures"
    );
    assert_eq!(
        ourios_semconv::OURIOS_MINER_PARAMS_OVERFLOW,
        "ourios.miner.params.overflow"
    );
    assert_eq!(
        ourios_semconv::OURIOS_MINER_TEMPLATE_VERSION_CHANGES,
        "ourios.miner.template.version_changes"
    );
    assert_eq!(
        ourios_semconv::OURIOS_MINER_CONFIDENCE,
        "ourios.miner.confidence"
    );
    assert_eq!(
        ourios_semconv::OURIOS_MINER_DURATION,
        "ourios.miner.duration"
    );
    assert_eq!(
        ourios_semconv::OURIOS_MINER_CONFIDENCE_P50,
        "ourios.miner.confidence.p50"
    );
    assert_eq!(
        ourios_semconv::OURIOS_MINER_CONFIDENCE_P01,
        "ourios.miner.confidence.p01"
    );
    assert_eq!(
        ourios_semconv::OURIOS_MINER_BODY_RETENTION_UTILIZATION,
        "ourios.miner.body_retention.utilization"
    );
    assert_eq!(
        ourios_semconv::OURIOS_MINER_PARAMS_OVERFLOW_UTILIZATION,
        "ourios.miner.params.overflow.utilization"
    );
    assert_eq!(ourios_semconv::OURIOS_SERVICE, "ourios.service");
    assert_eq!(
        ourios_semconv::OURIOS_MINER_TEMPLATE_CHANGE,
        "ourios.miner.template_change"
    );
}
