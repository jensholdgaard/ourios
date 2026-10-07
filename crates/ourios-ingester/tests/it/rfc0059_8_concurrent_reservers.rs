//! RFC0059.8 — Concurrent reservers on one store get disjoint blocks.
//! See `docs/rfcs/0059-durable-template-id-allocation.md` §5.

use ourios_ingester::template_ids::{self, HIGH_WATER_KEY, TemplateIdsError};
use ourios_parquet::Store;

const ROUNDS: usize = 40;

/// Scenario RFC0059.8 — two reservers on an If-Match store, many rounds,
/// receive disjoint blocks, and the high-water reads the highest end.
/// See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
#[test]
fn rfc0059_8_two_reservers_on_an_if_match_store_get_disjoint_blocks() {
    let store = Store::in_memory();
    assert!(store.supports_conditional_update());
    store
        .put_blocking(HIGH_WATER_KEY, br#"{"reserved_through": 0}"#.to_vec())
        .expect("seed");

    let reservers: Vec<_> = (0..2)
        .map(|_| {
            let store = store.clone();
            std::thread::spawn(move || {
                // A reservation that loses every compare-and-swap attempt is
                // a failed one the refiller retries; so does this reserver.
                let mut blocks = Vec::with_capacity(ROUNDS);
                while blocks.len() < ROUNDS {
                    match template_ids::reserve(&store, 0) {
                        Ok(block) => blocks.push(block),
                        Err(TemplateIdsError::Contended) => {}
                        Err(e) => panic!("reserve: {e}"),
                    }
                }
                blocks
            })
        })
        .collect();
    let mut blocks: Vec<_> = reservers
        .into_iter()
        .flat_map(|reserver| reserver.join().expect("reserver"))
        .collect();

    blocks.sort_by_key(|block| block.after());
    for pair in blocks.windows(2) {
        assert!(
            pair[0].through() <= pair[1].after(),
            "blocks overlap: {:?} and {:?}",
            pair[0],
            pair[1]
        );
    }
    let highest = blocks.iter().map(|b| b.through()).max().expect("blocks");
    let read = template_ids::read(&store)
        .expect("read")
        .expect("present")
        .reserved_through;
    assert_eq!(read, highest);
    assert_eq!(blocks.len(), 2 * ROUNDS);
}
