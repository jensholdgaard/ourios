use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex};

use super::*;
use crate::snapshot::{load_snapshot, snapshot};
use crate::upstream::LOG_RECORD_TEMPLATE_ATTR;

// ---------- RFC 0065 §6.5 miner configuration, at miner level ----------

static CORPUS: LazyLock<Vec<String>> = LazyLock::new(|| {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/corpus");
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .expect("testdata/corpus")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "txt"))
        .collect();
    files.sort();
    files
        .iter()
        .flat_map(|p| {
            std::fs::read_to_string(p)
                .expect("corpus file")
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .collect()
});

fn tenants() -> [TenantId; 2] {
    [TenantId::new("eq-perses"), TenantId::new("nocturnal")]
}

fn corpus_record(pick: usize) -> OtlpLogRecord {
    string_record(&tenants()[pick % 2], &CORPUS[pick % CORPUS.len()])
}

fn with_threshold(threshold: f32) -> MinerConfig {
    MinerConfig {
        similarity_threshold: threshold,
        similarity_floor: threshold.min(0.4),
        ..MinerConfig::default()
    }
}

fn observed(config: MinerConfig) -> (MinerCluster, SharedRecordSink) {
    let sink = SharedRecordSink::new();
    let cluster = MinerCluster::new(config).with_record_sink(Box::new(sink.clone()));
    (cluster, sink)
}

fn snapshots(cluster: &MinerCluster) -> Vec<(TenantId, SnapshotState)> {
    cluster
        .tenant_ids()
        .into_iter()
        .map(|tenant| {
            let bytes = snapshot(&cluster.snapshot_state(&tenant)).expect("encode");
            (tenant, load_snapshot(&bytes).expect("decode"))
        })
        .collect()
}

fn restore_all(cluster: &mut MinerCluster, states: &[(TenantId, SnapshotState)]) {
    for (tenant, state) in states {
        cluster.restore_tenant(tenant, state).expect("restore");
    }
}

fn mine(cluster: &mut MinerCluster, picks: &[usize]) {
    for &pick in picks {
        let _ = cluster.ingest(&corpus_record(pick));
    }
}

fn assert_same_state(left: &MinerCluster, right: &MinerCluster) -> Result<(), TestCaseError> {
    prop_assert_eq!(left.tenant_ids(), right.tenant_ids());
    for tenant in left.tenant_ids() {
        prop_assert_eq!(left.snapshot_state(&tenant), right.snapshot_state(&tenant));
        prop_assert_eq!(left.template_count(&tenant), right.template_count(&tenant));
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// RFC0065.31 at miner level — a miner that mined through a
    /// configuration change and one restored from a snapshot cut before
    /// it, reconfigured before or after the restore, mine the lines
    /// after the change into the same templates.
    #[test]
    fn a_reconfigured_miner_and_a_restored_one_mine_alike(
        picks in prop::collection::vec(0..1000usize, 1..300),
        cut in any::<prop::sample::Index>(),
        threshold in prop::sample::select(vec![0.4f32, 0.5, 0.6, 0.8, 0.9, 1.0]),
        max_templates in prop::sample::select(vec![3u32, 20, 20_000]),
    ) {
        let (before, after) = picks.split_at(cut.index(picks.len() + 1));
        let changed = MinerConfig { max_templates, ..with_threshold(threshold) };

        let (mut through, through_rows) = observed(with_threshold(0.7));
        mine(&mut through, before);
        let states = snapshots(&through);
        let _ = through_rows.drain();
        through.reconfigure(changed).expect("same shape");
        mine(&mut through, after);

        let (mut restored_first, restored_first_rows) = observed(with_threshold(0.7));
        restore_all(&mut restored_first, &states);
        restored_first.reconfigure(changed).expect("same shape");
        mine(&mut restored_first, after);

        let (mut reconfigured_first, reconfigured_first_rows) = observed(with_threshold(0.7));
        reconfigured_first.reconfigure(changed).expect("same shape");
        restore_all(&mut reconfigured_first, &states);
        mine(&mut reconfigured_first, after);

        let rows = through_rows.drain();
        prop_assert_eq!(&rows, &restored_first_rows.drain());
        prop_assert_eq!(&rows, &reconfigured_first_rows.drain());
        assert_same_state(&through, &restored_first)?;
        assert_same_state(&through, &reconfigured_first)?;
    }
}

#[test]
fn a_tree_shape_change_is_refused_and_changes_nothing() {
    let shallower = MinerConfig::default()
        .with_prefix_depth(1)
        .expect("in range");
    let narrower = MinerConfig::default()
        .with_max_node_children(7)
        .expect("in range");
    for requested in [shallower, narrower] {
        let mut cluster = MinerCluster::new(MinerConfig::default());
        let mut twin = MinerCluster::new(MinerConfig::default());
        mine(&mut cluster, &[0, 1, 2, 3]);
        mine(&mut twin, &[0, 1, 2, 3]);

        let err = cluster.reconfigure(requested).expect_err("shape change");
        assert_eq!(
            err,
            TreeShapeChanged {
                current: TreeShape::of(&MinerConfig::default()),
                requested: TreeShape::of(&requested),
            }
        );
        assert_eq!(*cluster.config(), MinerConfig::default());
        for tenant in cluster.tenant_ids() {
            assert_eq!(
                cluster.effective_config(&tenant),
                MinerConfig::default(),
                "{tenant:?}"
            );
        }
        for pick in 4..40 {
            assert_eq!(
                cluster.ingest(&corpus_record(pick)),
                twin.ingest(&corpus_record(pick)),
                "line {pick}"
            );
        }
        for tenant in twin.tenant_ids() {
            assert_eq!(
                cluster.snapshot_state(&tenant),
                twin.snapshot_state(&tenant)
            );
        }
    }
}

#[test]
fn reconfigure_reaches_live_and_later_tenants_but_not_overridden_ones() {
    let [live, overridden] = tenants();
    let later = TenantId::new("later");
    let own = with_threshold(0.9);
    let changed = with_threshold(0.5);
    let mut cluster =
        MinerCluster::new(MinerConfig::default()).with_tenant_config(overridden.clone(), own);
    let _ = cluster.ingest(&string_record(&live, "user alice logged in"));
    let _ = cluster.ingest(&string_record(&overridden, "user alice logged in"));

    cluster.reconfigure(changed).expect("same shape");

    assert_eq!(*cluster.config(), changed);
    assert_eq!(cluster.effective_config(&live), changed);
    assert_eq!(cluster.effective_config(&later), changed);
    assert_eq!(cluster.effective_config(&overridden), own);
}

#[test]
fn an_override_registered_for_a_live_tenant_has_no_effect_then_or_later() {
    let t = TenantId::new("tenant-x");
    let mut cluster = MinerCluster::new(with_threshold(0.7));
    let _ = cluster.ingest(&string_record(&t, "user alice logged in"));

    let mut cluster = cluster.with_tenant_config(t.clone(), with_threshold(0.9));
    assert_eq!(cluster.effective_config(&t), with_threshold(0.7));

    cluster
        .reconfigure(with_threshold(0.5))
        .expect("same shape");
    assert_eq!(cluster.effective_config(&t), with_threshold(0.5));
}

#[test]
fn an_override_registered_before_a_restore_is_the_restored_tenants_config() {
    let t = TenantId::new("tenant-x");
    let own = with_threshold(0.9);
    let mut original = MinerCluster::new(MinerConfig::default());
    let _ = original.ingest(&string_record(&t, "user alice logged in"));
    let state = original.snapshot_state(&t);

    let mut restored = MinerCluster::new(MinerConfig::default()).with_tenant_config(t.clone(), own);
    restored.restore_tenant(&t, &state).expect("restore");
    restored
        .reconfigure(with_threshold(0.5))
        .expect("same shape");

    assert_eq!(restored.effective_config(&t), own);
}

#[test]
fn a_ceiling_lowered_below_the_count_keeps_every_template_and_mints_none() {
    let t = TenantId::new("tenant-x");
    let mut cluster = MinerCluster::new(MinerConfig::default());
    let alpha = cluster.ingest(&string_record(&t, "alpha connected to replica"));
    let _ = cluster.ingest(&string_record(&t, "beta disk usage high"));
    let _ = cluster.ingest(&string_record(&t, "gamma queue drained"));
    let failures = cluster.parse_failures_total();

    let lowered = MinerConfig::default()
        .with_max_templates(1)
        .expect("non-zero");
    cluster.reconfigure(lowered).expect("same shape");

    assert_eq!(cluster.template_count(&t), 3, "no template is removed");
    assert_eq!(
        cluster.ingest(&string_record(&t, "alpha connected to replica")),
        alpha,
        "an existing template still attaches"
    );
    assert_eq!(
        cluster.ingest(&string_record(&t, "delta cache warmed up now")),
        NO_TEMPLATE,
        "at the ceiling a would-be mint is a parse failure"
    );
    assert_eq!(cluster.parse_failures_total(), failures + 1);
    assert_eq!(cluster.body_retentions_total(), failures + 1);
    assert_eq!(cluster.template_count(&t), 3);
}

fn annotated(tenant: &TenantId, body: &str, template: &str) -> OtlpLogRecord {
    let mut record = string_record(tenant, body);
    record.attributes.push(ourios_core::otlp::KeyValue {
        key: LOG_RECORD_TEMPLATE_ATTR.to_string(),
        value: Some(AnyValue {
            value: Some(AvValue::StringValue(template.to_string())),
        }),
        ..Default::default()
    });
    record
}

#[test]
fn leaving_adopt_keeps_the_adopted_templates_as_a_restore_under_the_new_mode_does() {
    let t = TenantId::new("tenant-x");
    let adopt = MinerConfig::default().with_upstream_templates(UpstreamTemplates::Adopt);
    let ignore = MinerConfig::default();
    let mut cluster = MinerCluster::new(adopt);
    let adopted = cluster.ingest(&annotated(&t, "user alice logged in", "user <*> logged in"));
    assert_ne!(adopted, NO_TEMPLATE);
    let state = cluster.snapshot_state(&t);

    cluster.reconfigure(ignore).expect("same shape");
    let mut restored = MinerCluster::new(ignore);
    restored.restore_tenant(&t, &state).expect("restore");

    assert_eq!(cluster.snapshot_state(&t), restored.snapshot_state(&t));
    assert_eq!(cluster.adopted_templates_for(&t).len(), 1);
    let later = [
        annotated(&t, "user bob logged out", "user <*> logged out"),
        string_record(&t, "user carol logged in"),
        string_record(&t, "user dave logged in"),
    ];
    for record in &later {
        assert_eq!(cluster.ingest(record), restored.ingest(record));
    }
    assert_eq!(
        cluster.adopted_templates_for(&t).len(),
        1,
        "nothing is adopted under ignore"
    );
    assert_eq!(cluster.snapshot_state(&t), restored.snapshot_state(&t));
}

#[test]
fn entering_adopt_adopts_from_the_next_line() {
    let t = TenantId::new("tenant-x");
    let mut cluster = MinerCluster::new(MinerConfig::default());
    let line = annotated(&t, "user alice logged in", "user <*> logged in");
    let mined = cluster.ingest(&line);
    assert!(cluster.adopted_templates_for(&t).is_empty());

    cluster
        .reconfigure(MinerConfig::default().with_upstream_templates(UpstreamTemplates::Adopt))
        .expect("same shape");

    assert_ne!(cluster.ingest(&line), NO_TEMPLATE);
    assert_eq!(cluster.adopted_templates_for(&t).len(), 1);
    assert!(
        cluster
            .templates_for(&t)
            .iter()
            .any(|l| l.template_id == mined),
        "the mined leaf stays"
    );
}

// ---------- the id allocator's cursor ----------

/// Hands out the queued blocks in order, counting the reservations.
#[derive(Clone)]
struct Blocks(Arc<Mutex<VecDeque<IdBlock>>>);

impl Blocks {
    fn new(blocks: &[(u64, u64)]) -> Self {
        Self(Arc::new(Mutex::new(
            blocks
                .iter()
                .map(|&(after, through)| IdBlock::new(after, through).expect("non-empty"))
                .collect(),
        )))
    }

    fn left(&self) -> usize {
        self.0.lock().expect("blocks").len()
    }
}

impl IdReserver for Blocks {
    fn reserve(&mut self, _floor: u64) -> Result<IdBlock, IdReservationError> {
        self.0
            .lock()
            .expect("blocks")
            .pop_front()
            .ok_or_else(|| IdReservationError::new("no block queued"))
    }
}

fn distinct_line(t: &TenantId, i: usize) -> OtlpLogRecord {
    let words = ["alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta"];
    string_record(t, &words[..=i].join(" "))
}

#[test]
fn the_id_cursor_round_trips_and_continues_with_the_same_ids() {
    let t = TenantId::new("tenant-x");
    let mut original = MinerCluster::new(MinerConfig::default())
        .with_id_reserver(Box::new(Blocks::new(&[(0, 4), (10, 12)])));
    let first: Vec<u64> = (0..2)
        .map(|i| original.ingest(&distinct_line(&t, i)))
        .collect();
    assert_eq!(first, [1, 2]);

    let cursor = original.id_cursor();
    assert_eq!(cursor, IdCursor::new(3, 5).expect("valid"));

    let resumed_blocks = Blocks::new(&[(10, 12)]);
    let mut resumed = MinerCluster::new(MinerConfig::default())
        .with_id_reserver(Box::new(resumed_blocks.clone()));
    resumed
        .restore_tenant(&t, &original.snapshot_state(&t))
        .expect("restore");
    resumed.restore_id_cursor(cursor).expect("ahead");
    assert_eq!(resumed.id_cursor(), cursor);

    let continued: Vec<u64> = (2..6)
        .map(|i| original.ingest(&distinct_line(&t, i)))
        .collect();
    let mut in_hand = Vec::new();
    for i in 2..4 {
        in_hand.push(resumed.ingest(&distinct_line(&t, i)));
        assert_eq!(
            resumed_blocks.left(),
            1,
            "the range in hand needs no reservation"
        );
    }
    let rest: Vec<u64> = (4..6)
        .map(|i| resumed.ingest(&distinct_line(&t, i)))
        .collect();
    assert_eq!(continued, [3, 4, 11, 12]);
    assert_eq!([in_hand, rest].concat(), continued);
}

#[test]
fn restoring_tenants_before_or_after_the_cursor_seats_the_allocator_alike() {
    let [t, _] = tenants();
    let mut original = MinerCluster::new(MinerConfig::default())
        .with_id_reserver(Box::new(Blocks::new(&[(0, 8)])));
    mine(&mut original, &[0, 2, 4, 6]);
    let state = original.snapshot_state(&t);
    let cursor = original.id_cursor();

    let mut tenants_first = MinerCluster::new(MinerConfig::default());
    tenants_first.restore_tenant(&t, &state).expect("restore");
    tenants_first.restore_id_cursor(cursor).expect("ahead");
    let mut cursor_first = MinerCluster::new(MinerConfig::default());
    cursor_first.restore_id_cursor(cursor).expect("fresh");
    cursor_first.restore_tenant(&t, &state).expect("restore");

    assert_eq!(tenants_first.id_cursor(), cursor);
    assert_eq!(cursor_first.id_cursor(), cursor);
}

#[test]
fn a_cursor_behind_an_allocated_id_is_refused_and_changes_nothing() {
    let t = TenantId::new("tenant-x");
    let mut cluster = MinerCluster::new(MinerConfig::default());
    for i in 0..3 {
        let _ = cluster.ingest(&distinct_line(&t, i));
    }
    let before = cluster.id_cursor();
    let behind = IdCursor::new(2, 100).expect("valid");

    assert_eq!(
        cluster.restore_id_cursor(behind),
        Err(IdCursorBehind {
            cursor: behind,
            highest_allocated: 3,
        })
    );
    assert_eq!(cluster.id_cursor(), before);
}

#[test]
fn a_used_up_range_exports_as_an_empty_one() {
    let t = TenantId::new("tenant-x");
    let mut cluster = MinerCluster::new(MinerConfig::default())
        .with_id_reserver(Box::new(Blocks::new(&[(0, 1)])));
    let _ = cluster.ingest(&distinct_line(&t, 0));
    cluster.allocate_past_issued(9).expect("seat");

    let cursor = cluster.id_cursor();
    assert_eq!((cursor.next(), cursor.end()), (10, 10));
}

#[test]
fn an_id_cursor_holds_only_ids_in_the_domain() {
    assert!(IdCursor::new(0, 5).is_none(), "0 is NO_TEMPLATE");
    assert!(IdCursor::new(6, 5).is_none());
    assert!(IdCursor::new(1, MAX_TEMPLATE_ID + 2).is_none());
    assert!(IdCursor::new(5, 5).is_some());
    assert!(IdCursor::new(MAX_TEMPLATE_ID + 1, MAX_TEMPLATE_ID + 1).is_some());
}
