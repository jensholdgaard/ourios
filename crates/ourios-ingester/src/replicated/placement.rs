//! Tenant-to-group routing and rendezvous placement (RFC 0065 §6.1).
//!
//! Both hashes are XXH3's 64-bit variant with seed 0. Their inputs are
//! fixed by the RFC and pinned by golden values (RFC0065.8): every node
//! must compute the same group for a tenant and the same members for a
//! group, so a change here moves tenants between groups.

use std::cmp::Reverse;
use std::num::NonZeroU32;

use twox_hash::XxHash3_64;

/// The group a tenant belongs to: `xxh3_64(tenant_bytes) mod groups`.
///
/// `tenant_bytes` are the tenant id's bytes exactly as the
/// `TenantOtlpBatch` header carries them (RFC 0046 §3.3), with no
/// normalisation.
#[must_use]
pub fn group_of(tenant_bytes: &[u8], groups: NonZeroU32) -> u32 {
    let group = XxHash3_64::oneshot(tenant_bytes) % u64::from(groups.get());
    // The remainder is below `groups`, itself a u32.
    #[allow(clippy::cast_possible_truncation)]
    let group = group as u32;
    group
}

/// The `n` candidates a group is placed on, highest score first, where a
/// candidate's score is `xxh3_64` over the group as a 4-byte big-endian
/// u32 followed by the node id's UTF-8 bytes (rendezvous hashing).
///
/// Equal scores rank the bytewise-smaller node id first, so the result
/// depends only on the set of candidates, not their order. Repeated ids
/// count once; fewer than `n` distinct candidates returns them all.
/// Used for a group's voters (from `peers`) and its tail replicas (from
/// `tail_peers`).
#[must_use]
pub fn place<'a>(
    group: u32,
    candidates: impl IntoIterator<Item = &'a str>,
    n: usize,
) -> Vec<&'a str> {
    let mut ranked: Vec<(Reverse<u64>, &str)> = candidates
        .into_iter()
        .map(|node_id| (Reverse(score(group, node_id)), node_id))
        .collect();
    ranked.sort_unstable();
    ranked.dedup_by_key(|&mut (_, node_id)| node_id);
    ranked
        .into_iter()
        .take(n)
        .map(|(_, node_id)| node_id)
        .collect()
}

fn score(group: u32, node_id: &str) -> u64 {
    let mut input = Vec::with_capacity(4 + node_id.len());
    input.extend_from_slice(&group.to_be_bytes());
    input.extend_from_slice(node_id.as_bytes());
    XxHash3_64::oneshot(&input)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const PEERS: [&str; 5] = [
        "receiver-0",
        "receiver-1",
        "receiver-2",
        "receiver-3",
        "receiver-4",
    ];
    const TAIL_PEERS: [&str; 2] = ["tail-0", "tail-1"];

    fn groups(n: u32) -> NonZeroU32 {
        NonZeroU32::new(n).expect("nonzero")
    }

    /// Scenario RFC0065.8 — routing and tenancy, the hash arm: the
    /// tenant and placement hashes on a fixed set of tenant ids and peers
    /// return their golden values. A failure here means nodes running
    /// different builds disagree on where a tenant lives.
    #[test]
    fn rfc0065_8_the_hashes_return_their_golden_values() {
        // XXH3-64 of the empty input with seed 0 is the reference vector.
        assert_eq!(XxHash3_64::oneshot(b""), 0x2d06_8005_38d3_94c2);
        assert_eq!(score(3, "receiver-2"), 0xad48_7665_aa8f_1a70);

        let tenants: [(&str, u32, u32); 6] = [
            ("", 2, 4),
            ("default", 10, 4),
            ("acme", 1, 2),
            ("tenant-a", 1, 6),
            ("tenant-b", 11, 3),
            ("caf\u{e9}", 15, 6),
        ];
        for (tenant, of_16, of_7) in tenants {
            assert_eq!(
                group_of(tenant.as_bytes(), groups(16)),
                of_16,
                "{tenant:?} of 16"
            );
            assert_eq!(
                group_of(tenant.as_bytes(), groups(7)),
                of_7,
                "{tenant:?} of 7"
            );
        }

        let placements: [(u32, [&str; 3], &str); 4] = [
            (0, ["receiver-4", "receiver-1", "receiver-0"], "tail-0"),
            (1, ["receiver-3", "receiver-2", "receiver-0"], "tail-1"),
            (2, ["receiver-0", "receiver-1", "receiver-4"], "tail-1"),
            (3, ["receiver-2", "receiver-4", "receiver-1"], "tail-1"),
        ];
        for (group, voters, tail) in placements {
            assert_eq!(place(group, PEERS, 3), voters, "voters of group {group}");
            assert_eq!(place(group, TAIL_PEERS, 1), [tail], "tail of group {group}");
        }
    }

    #[test]
    fn a_repeated_candidate_counts_once() {
        assert_eq!(place(0, ["b", "a", "b", "a"], 4), place(0, ["a", "b"], 2));
        assert_eq!(place(0, ["receiver-0"; 3], 3), ["receiver-0"]);
    }

    #[test]
    fn more_members_than_candidates_returns_every_candidate() {
        let mut placed = place(5, TAIL_PEERS, 3);
        placed.sort_unstable();
        assert_eq!(placed, TAIL_PEERS);
        assert!(place(5, PEERS, 0).is_empty());
        assert!(place(5, [], 3).is_empty());
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: ourios_testgen::proptest_cases(256),
            ..ProptestConfig::default()
        })]

        #[test]
        fn a_tenant_maps_below_the_group_count(
            tenant in proptest::collection::vec(any::<u8>(), 0..64),
            n in 1..=u32::MAX,
        ) {
            prop_assert!(group_of(&tenant, groups(n)) < n);
        }

        #[test]
        fn placement_returns_n_distinct_members_independent_of_order(
            group in any::<u32>(),
            (ids, shuffled) in proptest::collection::vec("[a-z0-9-]{1,12}", 0..12)
                .prop_flat_map(|ids| (Just(ids.clone()), Just(ids).prop_shuffle())),
            n in 0..8_usize,
        ) {
            let candidates: Vec<&str> = ids.iter().map(String::as_str).collect();
            let placed = place(group, candidates.iter().copied(), n);

            let mut distinct = candidates.clone();
            distinct.sort_unstable();
            distinct.dedup();
            prop_assert_eq!(placed.len(), n.min(distinct.len()));
            let mut unique = placed.clone();
            unique.sort_unstable();
            unique.dedup();
            prop_assert_eq!(unique.len(), placed.len());
            prop_assert!(placed.iter().all(|id| distinct.contains(id)));

            prop_assert_eq!(&place(group, candidates.iter().copied(), n), &placed);
            prop_assert_eq!(place(group, shuffled.iter().map(String::as_str), n), placed);
        }
    }
}
