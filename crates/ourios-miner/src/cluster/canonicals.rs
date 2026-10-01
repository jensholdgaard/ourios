//! The RFC0050.6 convergence guard's index of mined canonicals.

use std::collections::HashMap;

/// `(canonical, severity_number, scope_name)` — the adopted-template
/// map's key shape.
pub(super) type CanonicalKey = (String, u8, Option<String>);

/// How many of a tenant's mined leaves carry each canonical shape.
///
/// A count rather than a set: two leaves under distinct masked paths can
/// share one shape, and one of them widening away must leave the shape
/// indexed for the other. The index is then a function of the tree
/// alone, which is what lets a restored tenant rebuild it exactly.
#[derive(Debug, Default)]
pub(super) struct MinedCanonicals(HashMap<CanonicalKey, usize>);

impl MinedCanonicals {
    pub(super) fn insert(&mut self, key: CanonicalKey) {
        *self.0.entry(key).or_insert(0) += 1;
    }

    pub(super) fn contains(&self, key: &CanonicalKey) -> bool {
        self.0.contains_key(key)
    }

    /// One leaf's canonical changed from `old` to `new`.
    pub(super) fn replace(&mut self, old: &CanonicalKey, new: CanonicalKey) {
        if let Some(count) = self.0.get_mut(old) {
            *count -= 1;
            if *count == 0 {
                self.0.remove(old);
            }
        }
        self.insert(new);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(canonical: &str) -> CanonicalKey {
        (canonical.to_owned(), 9, None)
    }

    #[test]
    fn a_shared_shape_stays_indexed_until_its_last_leaf_moves_away() {
        let mut index = MinedCanonicals::default();
        index.insert(key("<*> did a b"));
        index.insert(key("<*> did a b"));

        index.replace(&key("<*> did a b"), key("<*> did a <*>"));
        assert!(index.contains(&key("<*> did a b")), "one leaf still has it");

        index.replace(&key("<*> did a b"), key("<*> did a <*>"));
        assert!(!index.contains(&key("<*> did a b")));
        assert!(index.contains(&key("<*> did a <*>")));
    }
}
