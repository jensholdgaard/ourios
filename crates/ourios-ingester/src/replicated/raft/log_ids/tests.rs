//! The lock's bytes and every refusal, and allocation by compare-and-swap.

use std::collections::BTreeSet;

use ourios_parquet::Store;

use super::{AllocateError, LOG_IDS_KEY, LockUnreadable, LogIds, Refused, allocate};

/// The lock §6.6 shows, as written there.
const RFC_EXAMPLE: &str =
    r#"{"format": 1, "bootstrapper": "ourios-receiver-0", "seeded": false, "next": 0}"#;

/// The same lock as the encoder writes it.
const GOLDEN: &str = r#"{"format":1,"bootstrapper":"ourios-receiver-0","seeded":false,"next":0}"#;

fn lock(seeded: bool, next: u32) -> LogIds {
    LogIds {
        bootstrapper: "ourios-receiver-0".to_owned(),
        seeded,
        next,
    }
}

#[test]
fn the_rfc_example_decodes_and_encodes_as_the_golden_bytes() {
    let decoded = LogIds::decode(RFC_EXAMPLE.as_bytes()).expect("the RFC example is readable");
    assert_eq!(decoded, lock(false, 0));
    let encoded = decoded.encode().expect("encode");
    assert_eq!(String::from_utf8(encoded).expect("UTF-8"), GOLDEN);
    let seeded = lock(true, 7).encode().expect("encode");
    assert_eq!(
        String::from_utf8(seeded).expect("UTF-8"),
        r#"{"format":1,"bootstrapper":"ourios-receiver-0","seeded":true,"next":7}"#
    );
    let max = lock(true, u32::MAX).encode().expect("encode");
    assert_eq!(LogIds::decode(&max), Ok(lock(true, u32::MAX)));
}

/// A lock that is not exactly the format-1 fields is refused with its
/// reason; no reader takes it.
#[test]
fn a_lock_not_of_the_lock_shape_is_refused_with_its_reason() {
    let cases = [
        ("not JSON", "next: 0".to_owned(), "malformed"),
        (
            "truncated",
            GOLDEN[..GOLDEN.len() - 2].to_owned(),
            "malformed",
        ),
        ("empty", String::new(), "malformed"),
        (
            "an array",
            r#"[1,"ourios-receiver-0",false,0]"#.to_owned(),
            "malformed",
        ),
        (
            "another format",
            r#"{"format":2,"next":0}"#.to_owned(),
            "unsupported_format",
        ),
        (
            "a string format",
            GOLDEN.replace(r#""format":1"#, r#""format":"1""#),
            "malformed",
        ),
        (
            "an unknown field",
            GOLDEN.replace(r#""next":0"#, r#""next":0,"extra":0"#),
            "malformed",
        ),
        (
            "a missing field",
            GOLDEN.replace(r#","seeded":false"#, ""),
            "malformed",
        ),
        (
            "a field named twice",
            GOLDEN.replace(r#""next":0"#, r#""next":5,"next":0"#),
            "malformed",
        ),
        (
            "a negative next",
            GOLDEN.replace(r#""next":0"#, r#""next":-1"#),
            "malformed",
        ),
        (
            "a next past u32",
            GOLDEN.replace(r#""next":0"#, r#""next":4294967296"#),
            "malformed",
        ),
        (
            "a fractional next",
            GOLDEN.replace(r#""next":0"#, r#""next":1.5"#),
            "malformed",
        ),
        (
            "a string seeded",
            GOLDEN.replace(r#""seeded":false"#, r#""seeded":"false""#),
            "malformed",
        ),
        (
            "a numeric bootstrapper",
            GOLDEN.replace(r#""ourios-receiver-0""#, "0"),
            "malformed",
        ),
    ];
    for (case, json, error_type) in cases {
        let refused = LogIds::decode(json.as_bytes()).expect_err(case);
        assert_eq!(refused.error_type(), error_type, "{case}: {refused}");
    }
    let bytes = LogIds::decode(&[0xff, 0xfe]).expect_err("bytes");
    assert_eq!(bytes.error_type(), "malformed");
}

async fn put(store: &Store, lock: &LogIds) {
    store
        .put(LOG_IDS_KEY, lock.encode().expect("encode"))
        .await
        .expect("put");
}

async fn stored(store: &Store) -> Option<Vec<u8>> {
    match store.get(LOG_IDS_KEY).await {
        Ok(bytes) => Some(bytes),
        Err(e) if e.is_not_found() => None,
        Err(e) => panic!("read the lock: {e}"),
    }
}

/// Each allocation hands out `next` and advances it by one, keeping the
/// bootstrapper and `seeded`.
#[tokio::test]
async fn an_allocation_hands_out_next_and_advances_it() {
    let store = Store::in_memory();
    put(&store, &lock(true, 0)).await;
    assert_eq!(allocate(&store).await.ok(), Some(0));
    assert_eq!(allocate(&store).await.ok(), Some(1));
    let after = stored(&store).await.expect("the lock stays");
    assert_eq!(LogIds::decode(&after), Ok(lock(true, 2)));
}

/// Allocations that race never hand out one id twice.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn racing_allocations_never_share_an_id() {
    let store = Store::in_memory();
    put(&store, &lock(true, 0)).await;
    let racers: Vec<_> = (0..16)
        .map(|_| {
            let store = store.clone();
            tokio::spawn(async move { allocate(&store).await })
        })
        .collect();
    let mut ids = BTreeSet::new();
    for racer in racers {
        let id = racer.await.expect("joined").expect("allocated");
        assert!(ids.insert(id), "id {id} allocated twice");
    }
    assert_eq!(ids, (0..16).collect());
    let after = stored(&store).await.expect("the lock stays");
    assert_eq!(LogIds::decode(&after), Ok(lock(true, 16)));
}

/// No log id exists before the high-water object is seeded: allocation
/// waits, and the lock is left alone.
#[tokio::test]
async fn an_unseeded_lock_allocates_nothing() {
    let store = Store::in_memory();
    put(&store, &lock(false, 0)).await;
    let refused = allocate(&store).await.expect_err("unseeded");
    assert!(
        matches!(&refused, AllocateError::Unseeded { bootstrapper } if bootstrapper == "ourios-receiver-0"),
        "{refused}"
    );
    assert_eq!(stored(&store).await, Some(GOLDEN.as_bytes().to_vec()));
}

/// An absent lock was deleted: allocation fails closed with
/// `LogIdsDeleted` and re-creates nothing.
#[tokio::test]
async fn an_absent_lock_is_deleted_and_never_re_created() {
    let store = Store::in_memory();
    let refused = allocate(&store).await.expect_err("absent");
    assert!(
        matches!(refused, AllocateError::Refused(Refused::Deleted)),
        "{refused}"
    );
    assert_eq!(stored(&store).await, None, "the lock was re-created");
}

/// An unreadable lock fails closed and is never replaced.
#[tokio::test]
async fn an_unreadable_lock_fails_closed_and_is_kept() {
    let store = Store::in_memory();
    let garbage = br#"{"format":2,"next":0}"#.to_vec();
    store.put(LOG_IDS_KEY, garbage.clone()).await.expect("put");
    let refused = allocate(&store).await.expect_err("unreadable");
    assert!(
        matches!(
            refused,
            AllocateError::Refused(Refused::Unreadable(LockUnreadable::Format(2)))
        ),
        "{refused}"
    );
    assert_eq!(stored(&store).await, Some(garbage));
}

/// The last id a `u32` holds is handed out; after it nothing is, since
/// `next` cannot advance past it.
#[tokio::test]
async fn the_last_id_is_allocated_and_then_none() {
    let store = Store::in_memory();
    put(&store, &lock(true, u32::MAX - 1)).await;
    assert_eq!(allocate(&store).await.ok(), Some(u32::MAX - 1));
    let refused = allocate(&store).await.expect_err("exhausted");
    assert!(
        matches!(refused, AllocateError::Refused(Refused::Exhausted)),
        "{refused}"
    );
    let after = stored(&store).await.expect("the lock stays");
    assert_eq!(LogIds::decode(&after), Ok(lock(true, u32::MAX)));
}

/// A store that cannot compare-and-swap allocates nothing.
#[tokio::test]
async fn a_store_without_conditional_updates_allocates_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::local(dir.path()).expect("local store");
    put(&store, &lock(true, 0)).await;
    let refused = allocate(&store).await.expect_err("no compare-and-swap");
    assert!(
        matches!(
            refused,
            AllocateError::Refused(Refused::NoConditionalUpdate)
        ),
        "{refused}"
    );
    let after = stored(&store).await.expect("the lock stays");
    assert_eq!(LogIds::decode(&after), Ok(lock(true, 0)));
}
