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
