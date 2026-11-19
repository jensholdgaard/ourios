//! The template definitions a tail replica sends with its rows (RFC 0065
//! §6.7), as the miner holds them.

use std::collections::{BTreeMap, BTreeSet};

use super::adoption::{adopt_config, annotated_record};
use super::*;

/// RFC 0065 §6.7: an adoption riding a leaf keeps binding the version it
/// matched after the leaf widens, so a later record it adopts still
/// carries that version, and the definitions name it with the tokens
/// the adoption recorded, as well as the widened version.
#[test]
fn definitions_name_the_version_a_tree_backed_adoption_still_binds() {
    let t = TenantId::new("tenant-x");
    let records = SharedRecordSink::new();
    let mut cluster = MinerCluster::new(adopt_config()).with_record_sink(Box::new(records.clone()));

    let id = cluster.ingest(&string_record(&t, "user 42 logged in"));
    let _ = cluster.ingest(&annotated_record(
        &t,
        "user 43 logged in",
        "user <*> logged in",
    ));
    let _ = cluster.ingest(&string_record(&t, "user 44 logged out"));
    let again = cluster.ingest(&annotated_record(
        &t,
        "user 46 logged in",
        "user <*> logged in",
    ));
    assert_eq!(again, id);
    let used: BTreeSet<(u64, u32)> = records
        .drain()
        .iter()
        .map(|record| (record.template_id, record.template_version))
        .collect();
    assert_eq!(
        used,
        BTreeSet::from([(id, 1), (id, 2)]),
        "the adoption still binds version 1"
    );

    let defined: BTreeMap<_, _> = cluster
        .template_definitions(&t, &used)
        .into_iter()
        .collect();
    assert_eq!(
        defined,
        BTreeMap::from([
            ((id, 1), "user <*> logged in".to_owned()),
            ((id, 2), "user <*> logged <*>".to_owned()),
        ])
    );
    let one = BTreeSet::from([(id, 2)]);
    assert_eq!(
        cluster.template_definitions(&t, &one),
        vec![((id, 2), "user <*> logged <*>".to_owned())],
        "only the versions wanted"
    );
}
