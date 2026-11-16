use ourios_config::MinerConfig;
use ourios_core::tenant::TenantId;
use ourios_miner::snapshot::{
    LogMark, SegmentMark, StructuredTemplateRecord, WalHighWater, snapshot,
};

use super::*;
use crate::replicated::raft::miner_config::body_of;

const LOG: u32 = 3;
const SEQ: u64 = 200;
const TERM: u64 = 4;

fn config() -> GroupMinerConfig {
    GroupMinerConfig::new(body_of(&MinerConfig::default())).expect("default config")
}

fn block(after: u64, through: u64) -> IdBlock {
    IdBlock::new(after, through).expect("test block")
}

const AT: CutId = CutId {
    log: LOG,
    seq: SEQ,
    term: TERM,
};

fn cut() -> Cut {
    Cut {
        id: AT,
        latest_block: Some(block(2000, 3000)),
        config: Some(config()),
    }
}

/// `tenant`'s `.snap`: one template, id 7, marked with `mark`.
fn snap(tenant: &str, mark: Option<WalHighWater>) -> Vec<u8> {
    let state = SnapshotState {
        leaves: vec![],
        structured_templates: vec![StructuredTemplateRecord {
            severity_number: 9,
            scope_name: Some(tenant.to_string()),
            event_name: None,
            template_id: 7,
        }],
        wal_high_water: mark,
        adopted_templates: vec![],
    };
    snapshot(&state).expect("snapshot encodes")
}

fn marked(tenant: &str) -> Vec<u8> {
    snap(
        tenant,
        Some(WalHighWater::Replicated(LogMark { log: LOG, seq: SEQ })),
    )
}

fn entry(tenant: &str) -> TenantEntry {
    TenantEntry::of(TenantId::new(tenant), config(), &marked(tenant))
}

fn sample() -> Bundle {
    let ids =
        Allocator::new(1500, Some(block(1000, 2000)), vec![block(2000, 3000)]).expect("allocator");
    Bundle::new(AT, ids, vec![entry("acme"), entry("beta")]).expect("bundle")
}

/// One tenant as `bundle.bin` lays it out.
struct RawTenant<'a> {
    tenant: &'a [u8],
    kind: u8,
    body: Vec<u8>,
    len: u64,
    crc: u32,
}

impl<'a> RawTenant<'a> {
    fn of(tenant: &'a str) -> Self {
        let snap = marked(tenant);
        Self {
            tenant: tenant.as_bytes(),
            kind: 0x04,
            body: config().body().to_bytes().to_vec(),
            len: snap.len() as u64,
            crc: crc32c::crc32c(&snap),
        }
    }
}

/// `bundle.bin` written field by field from the RFC's layout, so a test
/// can lay out what [`Bundle::encode`] never would.
struct Raw<'a> {
    format: u8,
    head: (u32, u64, u64),
    next: u64,
    current: (u64, u64),
    blocks: Vec<(u64, u64)>,
    tenants: Vec<RawTenant<'a>>,
}

impl Raw<'_> {
    fn sample() -> Self {
        Self {
            format: 1,
            head: (LOG, SEQ, TERM),
            next: 1500,
            current: (1000, 2000),
            blocks: vec![(2000, 3000)],
            tenants: vec![RawTenant::of("acme"), RawTenant::of("beta")],
        }
    }

    fn body(&self) -> Vec<u8> {
        let mut out = vec![self.format];
        out.extend_from_slice(&self.head.0.to_le_bytes());
        for word in [
            self.head.1,
            self.head.2,
            self.next,
            self.current.0,
            self.current.1,
        ] {
            out.extend_from_slice(&word.to_le_bytes());
        }
        out.extend_from_slice(&u32::try_from(self.blocks.len()).expect("few").to_le_bytes());
        for (after, through) in &self.blocks {
            out.extend_from_slice(&after.to_le_bytes());
            out.extend_from_slice(&through.to_le_bytes());
        }
        out.extend_from_slice(
            &u32::try_from(self.tenants.len())
                .expect("few")
                .to_le_bytes(),
        );
        for tenant in &self.tenants {
            out.extend_from_slice(
                &u16::try_from(tenant.tenant.len())
                    .expect("short")
                    .to_le_bytes(),
            );
            out.extend_from_slice(tenant.tenant);
            out.push(tenant.kind);
            out.extend_from_slice(&tenant.body);
            out.extend_from_slice(&tenant.len.to_le_bytes());
            out.extend_from_slice(&tenant.crc.to_le_bytes());
        }
        out
    }

    fn bytes(&self) -> Vec<u8> {
        sealed(self.body())
    }
}

fn sealed(mut body: Vec<u8>) -> Vec<u8> {
    let crc = crc32c::crc32c(&body);
    body.extend_from_slice(&crc.to_le_bytes());
    body
}

fn refusal(raw: &Raw<'_>) -> BundleError {
    Bundle::decode(&raw.bytes()).expect_err("refused")
}

#[test]
fn the_bundle_is_laid_out_as_specified() {
    assert_eq!(sample().encode().expect("encodes"), Raw::sample().bytes());
}

#[test]
fn a_bundle_round_trips_and_matches_its_cut() {
    let bundle = sample();

    let decoded = Bundle::decode(&bundle.encode().expect("encodes")).expect("decodes");

    assert_eq!(decoded, bundle);
    assert_eq!(decoded.check(&cut()), Ok(()));
}

#[test]
fn an_allocator_that_never_started_a_block_issues_one_next() {
    let raw = Raw {
        next: 1,
        current: (0, 0),
        blocks: vec![(0, 2000), (2000, 3000)],
        ..Raw::sample()
    };

    let bundle = Bundle::decode(&raw.bytes()).expect("decodes");

    assert_eq!(bundle.ids().current(), None);
    assert_eq!(bundle.ids().next(), 1);
    assert_eq!(bundle.check(&cut()), Ok(()));
}

#[test]
fn a_bundle_of_another_format_is_refused() {
    let raw = Raw {
        format: 2,
        ..Raw::sample()
    };

    assert_eq!(refusal(&raw), BundleError::Format(2));
}

#[test]
fn a_bundle_whose_crc_fails_is_refused() {
    let mut bytes = Raw::sample().bytes();
    bytes[10] ^= 1;

    assert_eq!(Bundle::decode(&bytes), Err(BundleError::Crc));
}

#[test]
fn a_short_or_overlong_bundle_is_refused() {
    let body = Raw::sample().body();
    let mut longer = body.clone();
    longer.push(0);

    assert_eq!(Bundle::decode(&[1, 2]), Err(BundleError::Truncated));
    assert_eq!(
        Bundle::decode(&sealed(body[..body.len() - 1].to_vec())),
        Err(BundleError::Truncated)
    );
    assert_eq!(Bundle::decode(&sealed(longer)), Err(BundleError::Trailing));
}

#[test]
fn a_next_of_zero_is_refused() {
    let raw = Raw {
        next: 0,
        current: (0, 0),
        blocks: vec![],
        ..Raw::sample()
    };

    assert!(matches!(refusal(&raw), BundleError::Next { next: 0, .. }));
}

#[test]
fn a_next_outside_the_current_block_is_refused() {
    for next in [1, 1000, 2002] {
        let raw = Raw {
            next,
            blocks: vec![],
            ..Raw::sample()
        };

        assert!(matches!(refusal(&raw), BundleError::Next { .. }), "{next}");
    }
}

#[test]
fn a_block_outside_the_id_space_is_refused() {
    let top = i64::MAX.unsigned_abs();
    for (after, through) in [(2000, 2000), (3000, 2000), (2000, top + 1)] {
        let raw = Raw {
            blocks: vec![(after, through)],
            ..Raw::sample()
        };

        assert_eq!(refusal(&raw), BundleError::Block { after, through });
    }
}

#[test]
fn a_pending_block_below_the_one_before_it_is_refused() {
    let below_current = Raw {
        blocks: vec![(1500, 3000)],
        ..Raw::sample()
    };
    let out_of_order = Raw {
        blocks: vec![(3000, 4000), (2000, 3000)],
        ..Raw::sample()
    };

    assert_eq!(
        refusal(&below_current),
        BundleError::Pending {
            after: 1500,
            floor: 2000
        }
    );
    assert_eq!(
        refusal(&out_of_order),
        BundleError::Pending {
            after: 2000,
            floor: 4000
        }
    );
}

#[test]
fn tenants_out_of_order_or_repeated_are_refused() {
    let unsorted = Raw {
        tenants: vec![RawTenant::of("beta"), RawTenant::of("acme")],
        ..Raw::sample()
    };
    let repeated = Raw {
        tenants: vec![RawTenant::of("acme"), RawTenant::of("acme")],
        ..Raw::sample()
    };

    assert_eq!(refusal(&unsorted), BundleError::TenantOrder);
    assert_eq!(refusal(&repeated), BundleError::TenantOrder);
}

#[test]
fn a_tenant_id_empty_too_long_or_not_utf8_is_refused() {
    let long = "t".repeat(MAX_TENANT_BYTES + 1);
    for tenant in [&b""[..], long.as_bytes(), &[0xFF, 0xFE][..]] {
        let raw = Raw {
            tenants: vec![RawTenant {
                tenant,
                ..RawTenant::of("acme")
            }],
            ..Raw::sample()
        };

        assert_eq!(refusal(&raw), BundleError::Tenant, "{tenant:?}");
    }
}

#[test]
fn a_configuration_of_an_unknown_kind_is_refused() {
    let raw = Raw {
        tenants: vec![RawTenant {
            kind: 0x05,
            ..RawTenant::of("acme")
        }],
        ..Raw::sample()
    };

    assert_eq!(refusal(&raw), BundleError::ConfigKind(0x05));
}

#[test]
fn a_configuration_body_its_checks_refuse_is_refused() {
    let mut body = config().body();
    body.similarity_threshold = 2.0;
    let raw = Raw {
        tenants: vec![RawTenant {
            body: body.to_bytes().to_vec(),
            ..RawTenant::of("acme")
        }],
        ..Raw::sample()
    };

    assert!(matches!(refusal(&raw), BundleError::Config(_)));
}

#[test]
fn a_bundle_of_another_log_cut_or_term_does_not_match() {
    for (log, seq, term) in [(4, SEQ, TERM), (LOG, 100, TERM), (LOG, SEQ, 5)] {
        let raw = Raw {
            head: (log, seq, term),
            ..Raw::sample()
        };
        let bundle = Bundle::decode(&raw.bytes()).expect("decodes");

        assert_eq!(
            bundle.check(&cut()),
            Err(BundleError::Cut {
                found: CutId { log, seq, term },
                expected: AT,
            })
        );
    }
}

/// RFC0065.31's cases: latest block `(1000, 2000]` with no pending block
/// and `next` 1, and a pending `(0, 2000]` against that latest block.
#[test]
fn an_allocator_unlike_the_records_latest_block_is_refused() {
    let started_at_one = Raw {
        next: 1,
        blocks: vec![],
        ..Raw::sample()
    };
    let pending_from_zero = Raw {
        next: 1,
        current: (0, 0),
        blocks: vec![(0, 2000)],
        ..Raw::sample()
    };
    let at_latest = Cut {
        latest_block: Some(block(1000, 2000)),
        ..cut()
    };

    assert!(matches!(
        refusal(&started_at_one),
        BundleError::Next { next: 1, .. }
    ));
    assert_eq!(
        Bundle::decode(&pending_from_zero.bytes())
            .expect("decodes")
            .check(&at_latest),
        Err(BundleError::LatestBlock {
            found: Some(block(0, 2000)),
            expected: Some(block(1000, 2000)),
        })
    );
    assert_eq!(
        sample().check(&Cut {
            latest_block: None,
            ..cut()
        }),
        Err(BundleError::LatestBlock {
            found: Some(block(2000, 3000)),
            expected: None,
        })
    );
}

#[test]
fn a_tenant_configuration_unlike_the_one_in_force_is_refused() {
    let mut body = config().body();
    body.similarity_threshold = 0.8;
    let in_force = GroupMinerConfig::new(body).expect("valid");

    assert_eq!(
        sample().check(&Cut {
            config: Some(in_force),
            ..cut()
        }),
        Err(BundleError::TenantConfig(TenantId::new("acme")))
    );
    assert_eq!(
        sample().check(&Cut {
            config: None,
            ..cut()
        }),
        Err(BundleError::TenantConfig(TenantId::new("acme")))
    );
}

#[test]
fn a_snapshot_the_bundle_names_restores() {
    let bundle = sample();
    let acme = &bundle.tenants()[0];

    let state = bundle.snapshot(acme, &marked("acme")).expect("restores");

    assert_eq!(
        state.wal_high_water,
        Some(WalHighWater::Replicated(LogMark { log: LOG, seq: SEQ }))
    );
}

#[test]
fn a_snapshot_of_another_length_or_crc_is_refused() {
    let bundle = sample();
    let acme = &bundle.tenants()[0];
    let mut longer = marked("acme");
    longer.push(b' ');
    let mut flipped = marked("acme");
    let last = flipped.len() - 2;
    flipped[last] ^= 1;
    let tenant = TenantId::new("acme");

    assert!(matches!(
        bundle.snapshot(acme, &longer),
        Err(BundleError::SnapshotLength { .. })
    ));
    assert_eq!(
        bundle.snapshot(acme, &flipped),
        Err(BundleError::SnapshotCrc(tenant))
    );
}

#[test]
fn a_snapshot_load_snapshot_refuses_is_refused() {
    let bundle = sample();
    for bytes in [vec![2_u8, b'{', b'}'], vec![3_u8, b'{']] {
        let entry = TenantEntry::of(TenantId::new("acme"), config(), &bytes);

        assert!(
            matches!(
                bundle.snapshot(&entry, &bytes),
                Err(BundleError::Snapshot { .. })
            ),
            "{bytes:?}"
        );
    }
}

#[test]
fn a_snapshot_not_marked_with_the_cut_is_refused() {
    let bundle = sample();
    let local = WalHighWater::Local(SegmentMark {
        segment: "0190b3c8-1a2b-7c3d-9e4f-50607080a0b0".to_string(),
        byte: 64,
    });
    let marks = [
        None,
        Some(local),
        Some(WalHighWater::Replicated(LogMark { log: 4, seq: SEQ })),
        Some(WalHighWater::Replicated(LogMark { log: LOG, seq: 100 })),
    ];
    for mark in marks {
        let bytes = snap("acme", mark.clone());
        let entry = TenantEntry::of(TenantId::new("acme"), config(), &bytes);

        assert_eq!(
            bundle.snapshot(&entry, &bytes),
            Err(BundleError::SnapshotMark(TenantId::new("acme"))),
            "{mark:?}"
        );
    }
}

#[test]
fn a_snapshot_holding_an_id_at_or_above_next_is_refused() {
    let bytes = marked("acme");
    let id = ourios_miner::snapshot::load_snapshot(&bytes)
        .expect("decodes")
        .highest_template_id()
        .expect("one template");
    let ids = Allocator::new(id, Some(block(id - 1, id + 10)), vec![]).expect("allocator");
    let bundle = Bundle::new(
        AT,
        ids,
        vec![TenantEntry::of(TenantId::new("acme"), config(), &bytes)],
    )
    .expect("bundle");

    assert_eq!(
        bundle.snapshot(&bundle.tenants()[0], &bytes),
        Err(BundleError::SnapshotId {
            tenant: TenantId::new("acme"),
            id,
            next: id,
        })
    );
}

#[test]
fn keys_follow_the_layout() {
    assert_eq!(
        prefix(LOG, SEQ),
        "wal/replicated/logs/3/checkpoints/00000000000000000200/"
    );
    assert_eq!(
        bundle_key(LOG, SEQ),
        "wal/replicated/logs/3/checkpoints/00000000000000000200/bundle.bin"
    );
    assert_eq!(
        snapshot_key(LOG, SEQ, &TenantId::new("acme")),
        "wal/replicated/logs/3/checkpoints/00000000000000000200/tenants/acme.snap"
    );
    assert_eq!(pointer::key(LOG), "wal/replicated/logs/3/checkpoint.json");
}

const POINTER: &str = r#"{"format":1,"log":3,"seq":200,"term":4,"bundle":"wal/replicated/logs/3/checkpoints/00000000000000000200/"}"#;

#[test]
fn checkpoint_json_is_laid_out_as_specified() {
    assert_eq!(pointer::encode(AT), POINTER.as_bytes());
    assert_eq!(pointer::decode(POINTER.as_bytes(), LOG), Ok(AT));
}

#[test]
fn checkpoint_json_with_a_field_missing_unknown_or_repeated_is_unreadable() {
    let objects = [
        r#"{"format":1,"log":3,"seq":200,"term":4}"#.to_string(),
        POINTER.replace('}', r#","published_high":7}"#),
        POINTER.replace(r#""term":4"#, r#""term":4,"term":5"#),
        POINTER.replace(r#""seq":200"#, r#""seq":200.0"#),
        POINTER.replace(r#""seq":200"#, r#""seq":-1"#),
        POINTER.replace(r#""log":3"#, r#""log":4294967296"#),
        "not json".to_string(),
        r#"[1,3,200,4,"wal/replicated/logs/3/checkpoints/00000000000000000200/"]"#.to_string(),
    ];
    for object in objects {
        assert!(
            matches!(
                pointer::decode(object.as_bytes(), LOG),
                Err(pointer::PointerError::Malformed(_))
            ),
            "{object}"
        );
    }
}

#[test]
fn checkpoint_json_of_another_format_log_or_bundle_is_unreadable() {
    let other_format = POINTER.replace(r#""format":1"#, r#""format":2,"published_high":7"#);
    let other_bundle = POINTER.replace("00000000000000000200", "00000000000000000100");

    assert_eq!(
        pointer::decode(other_format.as_bytes(), LOG),
        Err(pointer::PointerError::Format(2))
    );
    assert_eq!(
        pointer::decode(POINTER.as_bytes(), 4),
        Err(pointer::PointerError::Log {
            found: 3,
            expected: 4
        })
    );
    assert!(matches!(
        pointer::decode(other_bundle.as_bytes(), LOG),
        Err(pointer::PointerError::Bundle(_))
    ));
}
