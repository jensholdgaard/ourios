//! #895 / #853 — a cold template-map derivation holds O(live template
//! state), not O(audit history).
//!
//! A crash loop that re-mints templates inflates a tenant's audit stream
//! without growing its registry: every restart re-emits creations and
//! widenings for the same `(template_id, version)` keys. The fold result
//! is bounded by those keys; the derivation's peak heap must be too, or a
//! one-hour query on a cold cache reads the tenant's whole audit history
//! into memory and OOMs the node.
//!
//! The assertion is on heap bytes, measured by `dhat`'s testing-mode
//! global allocator — deterministic, not wall RSS. Installing a global
//! allocator is process-global state, so this test lives in its own
//! integration binary (the RFC0028.2 process-isolation exemption, as
//! `rfc0033_7_observability.rs` is for the global `MeterProvider`).

use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};

use ourios_core::audit::{
    AuditEvent, AuditPayload, AuditSink, TemplateChange, hash_triggering_line,
};
use ourios_core::tenant::TenantId;
use ourios_parquet::{ParquetAuditSink, Store};
use ourios_querier::{StoreRef, derive_alias_map, derive_template_map, derive_template_registry};
use tempfile::TempDir;

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

const TENANT: &str = "crashloop";
/// A synthetic ordering anchor (2026-09-24T00:00:00Z); only the relative
/// order of the re-mints matters.
const TS0: u64 = 1_790_208_000_000_000_000;
const MINUTE_NS: u64 = 60_000_000_000;
/// Distinct `(template_id, version)` keys the history re-mints.
const LIVE_TEMPLATES: u64 = 2;
/// Audit files in the history — one event each through the production
/// sink, the per-restart flush shape.
const AUDIT_FILES: u64 = 160;
/// Tokens per template: ~20 KiB of template text per event.
const TOKENS: usize = 2_500;

/// A long canonical template, distinct per event so no two decoded
/// events share an allocation.
fn template(seq: u64) -> String {
    (0..TOKENS)
        .map(|i| {
            if i % 4 == 0 {
                "<*>".to_owned()
            } else {
                format!("t{seq}x{i}")
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Restart `seq` re-mints template `(seq % LIVE_TEMPLATES) + 1` at
/// version 1 — the fold's key never changes, only the history grows.
fn reminted(seq: u64) -> AuditEvent {
    AuditEvent {
        tenant_id: TenantId::new(TENANT),
        timestamp: UNIX_EPOCH + Duration::from_nanos(TS0 + seq * MINUTE_NS),
        payload: AuditPayload::Template {
            template_id: (seq % LIVE_TEMPLATES) + 1,
            triggering_line_hash: hash_triggering_line(b"line"),
            triggering_line_sample: None,
            change: TemplateChange::Created {
                new_template: template(seq),
            },
        },
    }
}

fn write_history(bucket: &Path) -> u64 {
    let mut sink = ParquetAuditSink::new(Store::local(bucket).expect("store"));
    let mut template_bytes = 0u64;
    for seq in 0..AUDIT_FILES {
        let event = reminted(seq);
        if let AuditPayload::Template {
            change: TemplateChange::Created { new_template },
            ..
        } = &event.payload
        {
            template_bytes += new_template.len() as u64;
        }
        sink.emit(event);
    }
    assert_eq!(sink.write_failures(), 0, "audit fixtures must all persist");
    template_bytes
}

/// Peak heap growth (bytes) while `work` runs.
fn peak_heap_during<T>(work: impl FnOnce() -> T) -> (T, u64) {
    let _profiler = dhat::Profiler::builder().testing().build();
    let out = work();
    let stats = dhat::HeapStats::get();
    (out, stats.max_bytes as u64)
}

#[test]
fn cold_derivation_peak_heap_is_bounded_by_live_templates_not_history() {
    let bucket = TempDir::new().expect("temp dir");
    let history_template_bytes = write_history(bucket.path());
    let tenant = TenantId::new(TENANT);
    let backend = StoreRef::Local(bucket.path());

    // A budget a quarter of the history's template text: far above what
    // the live state plus one audit file needs, far below holding the
    // history.
    let budget = history_template_bytes / 4;

    let ((map, bytes_read), peak) =
        peak_heap_during(|| derive_template_map(backend, &tenant).expect("derive template map"));
    eprintln!(
        "#895 derive_template_map: {AUDIT_FILES} audit files, {history_template_bytes} B of \
         template text, {bytes_read} B read, peak heap {peak} B (budget {budget} B)",
    );
    assert!(
        peak < budget,
        "the cold derivation held {peak} B at peak — O(audit history), not O(live templates) \
         (budget {budget} B)",
    );
    assert_eq!(map.registry().len() as u64, LIVE_TEMPLATES);
    for id in 1..=LIVE_TEMPLATES {
        let last = (0..AUDIT_FILES)
            .rev()
            .find(|seq| (seq % LIVE_TEMPLATES) + 1 == id)
            .expect("every live template is minted");
        assert_eq!(
            map.registry().get(&(id, 1)),
            Some(&ourios_miner::tree::parse_template(&template(last))),
            "template {id} folds to its latest re-mint",
        );
    }
    assert_eq!(map.folded_files().len() as u64, AUDIT_FILES);
    drop(map);

    let (registry, peak) = peak_heap_during(|| {
        derive_template_registry(backend, &tenant).expect("derive template registry")
    });
    assert!(
        peak < budget,
        "the registry derivation held {peak} B (budget {budget} B)"
    );
    assert_eq!(registry.len() as u64, LIVE_TEMPLATES);
    drop(registry);

    let (aliases, peak) =
        peak_heap_during(|| derive_alias_map(backend, &tenant).expect("derive alias map"));
    assert!(
        peak < budget,
        "the alias derivation held {peak} B (budget {budget} B)"
    );
    assert!(aliases.classes(&tenant).is_empty());
}
