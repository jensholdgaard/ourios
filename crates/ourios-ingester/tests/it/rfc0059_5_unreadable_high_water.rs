//! RFC0059.5 — An unreadable high-water fails startup closed.
//! RFC0059.11 — Any later-version high-water fails startup closed, even
//! beside a v1.
//! See `docs/rfcs/0059-durable-template-id-allocation.md` §5.

use ourios_ingester::recovery::RecoveryDriverError;
use ourios_ingester::template_ids::{HIGH_WATER_KEY, SEATED_MARKER, TemplateIdsError};

use crate::rfc0059_support::Node;

const LATER: &str = "miner/template_ids.v2.json";

/// Scenario RFC0059.5 — each unreadable object fails startup, naming the
/// object, and is not rewritten.
/// See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
#[test]
fn rfc0059_5_each_unreadable_object_fails_startup_before_any_listener() {
    for body in [
        &b"not json"[..],
        br#"{"other": 1}"#,
        br#"{"reserved_through": -1}"#,
        br#"{"reserved_through": "7"}"#,
    ] {
        let tmp = tempfile::TempDir::new().expect("temp");
        let node = Node::empty(tmp.path());
        node.put(HIGH_WATER_KEY, body);

        let Err(err) = node.restart() else {
            panic!("{body:?} must fail startup");
        };

        assert!(
            matches!(
                err,
                RecoveryDriverError::TemplateIds(TemplateIdsError::Malformed { .. })
            ),
            "{err}"
        );
        assert!(err.to_string().contains(HIGH_WATER_KEY), "{err}");
        assert_eq!(
            node.high_water_bytes().as_deref(),
            Some(body),
            "not rewritten"
        );
    }
}

/// Scenario RFC0059.11 — a later-version key fails startup alone and
/// beside a readable v1, and nothing is written.
/// See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
#[test]
fn rfc0059_11_a_later_version_fails_startup_alone_and_beside_v1() {
    for beside_v1 in [false, true] {
        let tmp = tempfile::TempDir::new().expect("temp");
        let node = Node::empty(tmp.path());
        let v1 = br#"{"reserved_through": 40}"#;
        if beside_v1 {
            node.put(HIGH_WATER_KEY, v1);
        }
        node.put(LATER, br#"{"reserved_through": 9000}"#);

        let Err(err) = node.restart() else {
            panic!("a later version must fail startup");
        };

        assert!(
            matches!(
                err,
                RecoveryDriverError::TemplateIds(TemplateIdsError::LaterVersion { .. })
            ),
            "{err}"
        );
        assert!(err.to_string().contains(LATER), "{err}");
        let expected_v1 = beside_v1.then_some(&v1[..]);
        assert_eq!(node.high_water_bytes().as_deref(), expected_v1, "no write");
        assert!(
            !node.snapshots.join(SEATED_MARKER).exists(),
            "no marker is written"
        );
    }
}
