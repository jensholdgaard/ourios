//! Drain prefix tree (RFC 0001 §6.2 step 3).
//!
//! Lays out the parent path that `MinerCluster::ingest` walks
//! before per-leaf `simSeq` selection. The shape is the
//! Drain-paper canonical one: root → length-N node → prefix-token
//! nodes → leaf list.
//!
//! # Scope of this module
//!
//! This is the **skeleton** PR. It ships the data structures and
//! [`Tree::descend_mut`] only. The integration that consumes the
//! parent (best-candidate selection via [`crate::sim_seq::sim_seq`],
//! the §6.2 step-5 widening branch, audit emission) is a future
//! PR; until then the tree exists but no caller walks it. Same
//! pattern as the [`crate::sim_seq`] roll-out — primitive lands
//! first in isolation, integration follows once the primitive is
//! settled.
//!
//! # Depth convention
//!
//! [`Tree::descend_mut`] takes `prefix_depth` as the **number of
//! prefix-token levels below the length node** (the Drain-paper
//! `d - 2` quantity), not the RFC §6.2-step-3 literal `d - 1`.
//! The two differ by one: the RFC's `L_masked[0..d-1]` notation
//! consumes one more token than the original Drain paper's
//! "first `d - 2` tokens." This module follows the paper because
//! the paper's interpretation lets the off-by-one stay out of the
//! API surface — the parameter means exactly what it's named, no
//! arithmetic is required at the call site. A follow-up RFC patch
//! will reconcile §6.2's wording with this convention; until then
//! the difference is a documentation issue, not a behavioural
//! one. [`DEFAULT_PREFIX_DEPTH`] = 2 corresponds to Drain3's
//! default `depth = 4`.
//!
//! # Wildcards in stored templates
//!
//! Leaves store [`OwnedToken`]s rather than [`crate::sim_seq::Token`]
//! borrows: a stored template needs to outlive any single ingest
//! call. The two share the same Fixed/Wildcard distinction; the
//! [`OwnedToken::as_borrowed`] helper hands a leaf's template to
//! [`crate::sim_seq::sim_seq`] without re-allocating. The
//! sentinel-string risk that [`crate::sim_seq`] calls out applies
//! identically here, so the wildcard stays a typed variant.

use std::collections::HashMap;

use crate::sim_seq::Token;

/// One position in a stored template — the owned counterpart to
/// [`crate::sim_seq::Token`]. Tree leaves outlive the ingest call
/// that created them, so they cannot hold borrowed slices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnedToken {
    Fixed(String),
    Wildcard,
}

impl OwnedToken {
    /// Borrow as a [`Token`] suitable for [`crate::sim_seq::sim_seq`].
    /// Zero-copy: the returned `Token::Fixed` references this
    /// `OwnedToken`'s inner `String`.
    #[must_use]
    pub fn as_borrowed(&self) -> Token<'_> {
        match self {
            Self::Fixed(s) => Token::Fixed(s.as_str()),
            Self::Wildcard => Token::Wildcard,
        }
    }
}

/// One entry in a [`PrefixNode`]'s leaf list.
///
/// Carries the [`OwnedToken`] template and its `template_id`. The
/// follow-up integration PR will add `version`, `slot_types`,
/// retained-body counts, and the rest of the §6.1 leaf payload;
/// they are deliberately absent here so the skeleton stays
/// reviewable.
#[derive(Debug, Clone)]
pub struct Leaf {
    pub template: Vec<OwnedToken>,
    /// Cluster-wide unique identifier per RFC 0001 §6.1 — the
    /// `template_id` allocated by [`crate::cluster::MinerCluster`]
    /// when the leaf was first created. Field name matches RFC
    /// language so future `template_version`, slot-id, and
    /// alias-id additions stay disambiguated.
    pub template_id: u64,
}

/// Internal node at a prefix-token level (or the per-length root).
///
/// `children` is keyed on the masked token at this level. `leaves`
/// holds the candidate set at the deepest prefix level reached
/// by [`Tree::descend_mut`]. By **convention** intermediate nodes
/// carry an empty `leaves` — the type does not enforce this; the
/// field is `pub` so the upcoming integration PR can push into
/// the node `descend_mut` returns. Pushing into an intermediate
/// node would be a caller bug.
#[derive(Debug, Default)]
pub struct PrefixNode {
    pub children: HashMap<String, PrefixNode>,
    pub leaves: Vec<Leaf>,
}

/// Length-grouping node — Drain partitions templates by token
/// count first so [`crate::sim_seq::sim_seq`]'s equal-length
/// precondition is upheld by tree construction rather than at
/// the call site.
#[derive(Debug, Default)]
pub struct LengthNode {
    pub root: PrefixNode,
}

/// Root of the Drain prefix tree. One per tenant in the eventual
/// integration; this module stays tenant-agnostic.
#[derive(Debug, Default)]
pub struct Tree {
    pub by_length: HashMap<usize, LengthNode>,
}

/// Drain3 default depth = 4 (root → length → prefix → prefix →
/// leaves), which leaves 2 prefix-token levels — see the
/// module-level "Depth convention" note.
pub const DEFAULT_PREFIX_DEPTH: usize = 2;

impl Tree {
    /// Build an empty tree.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Walk to the [`PrefixNode`] that owns the leaf list for
    /// `masked`, creating any missing nodes along the way.
    ///
    /// Returned node's `leaves` is the candidate set the caller
    /// will run [`crate::sim_seq::sim_seq`] over (in the
    /// integration PR). When `masked.len() < prefix_depth` the
    /// walk stops early — the entire line is consumed as path,
    /// so short lines bottom out at a **shallower** prefix level
    /// than long ones. This matches the Drain paper.
    ///
    /// # Panics
    ///
    /// If `masked` is empty. The miner's tokenize → mask path
    /// guarantees at least one token before any tree call; an
    /// empty input here means a caller bypassed that path.
    pub fn descend_mut(&mut self, masked: &[&str], prefix_depth: usize) -> &mut PrefixNode {
        assert!(
            !masked.is_empty(),
            "descend_mut precondition: masked must be non-empty (tokenize+mask guarantee N ≥ 1)",
        );

        let length = masked.len();
        let length_node = self.by_length.entry(length).or_default();

        // Walk only as many levels as the line has tokens —
        // shorter lines bottom out before exhausting prefix_depth.
        let walk_depth = prefix_depth.min(length);
        let path = &masked[..walk_depth];

        descend_recursively(&mut length_node.root, path)
    }
}

/// Recursive helper for [`Tree::descend_mut`]. Recursion is bounded
/// by `prefix_depth` (Drain default 2; a configurable cap of ~8 is
/// the realistic ceiling), so stack depth is a non-issue. The
/// recursion is here, rather than in an inline loop, because
/// re-binding `&mut PrefixNode` inside a `for` loop runs into the
/// stable borrow checker's well-known sub-borrow extension issue
/// (Polonius would solve it; until then recursion is the
/// safe-code idiom).
///
/// Hot-path allocation: the steady state once a tenant's templates
/// settle is that every prefix child already exists. Looking the
/// child up via `&str` `contains_key` first lets us call
/// [`String::to_string`] only on first-observation of a prefix
/// shape. `std::collections::HashMap`'s `Entry` API requires an
/// owned key, so the lookup-then-insert is the standard workaround
/// on stable; the second `get_mut` is one extra hash, never an
/// extra allocation.
fn descend_recursively<'t>(node: &'t mut PrefixNode, path: &[&str]) -> &'t mut PrefixNode {
    if path.is_empty() {
        return node;
    }
    let (head, tail) = path.split_first().expect("non-empty by guard above");

    if !node.children.contains_key(*head) {
        node.children
            .insert((*head).to_string(), PrefixNode::default());
    }
    let child = node
        .children
        .get_mut(*head)
        .expect("just inserted above if missing");
    descend_recursively(child, tail)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descend_mut_creates_length_node_lazily() {
        // Arrange — fresh tree, no length nodes yet.
        let mut tree = Tree::new();
        assert!(tree.by_length.is_empty());

        // Act
        let _ = tree.descend_mut(&["user", "logged", "in"], DEFAULT_PREFIX_DEPTH);

        // Assert — exactly one length bucket allocated, for N=3.
        assert_eq!(tree.by_length.len(), 1);
        assert!(tree.by_length.contains_key(&3));
    }

    #[test]
    fn descend_mut_groups_lines_of_same_length_under_one_length_node() {
        // Arrange — two lines of length 3 with different prefixes.
        let mut tree = Tree::new();

        // Act
        let _ = tree.descend_mut(&["user", "logged", "in"], DEFAULT_PREFIX_DEPTH);
        let _ = tree.descend_mut(&["GET", "/api", "200"], DEFAULT_PREFIX_DEPTH);

        // Assert — same length bucket, distinct first-prefix
        // children under it.
        assert_eq!(tree.by_length.len(), 1);
        let length_3 = tree.by_length.get(&3).expect("length-3 bucket present");
        assert_eq!(length_3.root.children.len(), 2);
        assert!(length_3.root.children.contains_key("user"));
        assert!(length_3.root.children.contains_key("GET"));
    }

    #[test]
    fn descend_mut_partitions_lines_of_different_length() {
        // Arrange — one length-2 line, one length-4 line.
        let mut tree = Tree::new();

        // Act
        let _ = tree.descend_mut(&["GET", "/home"], DEFAULT_PREFIX_DEPTH);
        let _ = tree.descend_mut(&["user", "42", "logged", "in"], DEFAULT_PREFIX_DEPTH);

        // Assert — distinct length buckets, no cross-pollination.
        assert_eq!(tree.by_length.len(), 2);
        assert!(tree.by_length.contains_key(&2));
        assert!(tree.by_length.contains_key(&4));
    }

    #[test]
    fn descend_mut_returns_same_node_for_repeat_calls() {
        // Arrange — two lines that share the (length, prefix)
        // shape: same length 4, same first two tokens.
        let mut tree = Tree::new();
        let line_a = ["user", "42", "logged", "in"];
        let line_b = ["user", "42", "logged", "out"];

        // Act — first call materialises the path; record its
        // leaf-list address. Second call must return a node whose
        // leaf-list is the same allocation (so a leaf pushed by
        // call 1 is visible to call 2).
        let leaves_addr_a = {
            let parent = tree.descend_mut(&line_a, DEFAULT_PREFIX_DEPTH);
            parent.leaves.push(Leaf {
                template: [OwnedToken::Fixed("marker".to_string())].into(),
                template_id: 99,
            });
            std::ptr::from_ref(&parent.leaves)
        };
        let parent_b = tree.descend_mut(&line_b, DEFAULT_PREFIX_DEPTH);
        let leaves_addr_b = std::ptr::from_ref(&parent_b.leaves);

        // Assert — same parent (pointer identity on the leaves
        // vector), and the marker leaf from call 1 is observable
        // from call 2.
        assert_eq!(leaves_addr_a, leaves_addr_b);
        assert_eq!(parent_b.leaves.len(), 1);
        assert_eq!(parent_b.leaves[0].template_id, 99);
    }

    #[test]
    fn descend_mut_walks_full_line_when_shorter_than_prefix_depth() {
        // Arrange — line of length 1, prefix_depth of 2; the
        // walk must stop at the line length, not over-consume.
        let mut tree = Tree::new();

        // Act
        let _ = tree.descend_mut(&["hello"], 2);

        // Assert — single length bucket for N=1, single child
        // ("hello") under its root, no further descent.
        let length_1 = tree.by_length.get(&1).expect("length-1 bucket present");
        assert_eq!(length_1.root.children.len(), 1);
        let hello = length_1
            .root
            .children
            .get("hello")
            .expect("first-token child present");
        assert!(
            hello.children.is_empty(),
            "must not descend past line length, got children: {:?}",
            hello.children.keys().collect::<Vec<_>>(),
        );
    }

    #[test]
    fn descend_mut_with_zero_prefix_depth_returns_length_root() {
        // Arrange — depth 0 means "skip the prefix path entirely"
        // (every line of the same length shares one leaf list).
        // Used by the integration tests / by callers that want to
        // disable prefix grouping.
        let mut tree = Tree::new();

        // Act
        let _ = tree.descend_mut(&["a", "b"], 0);
        let _ = tree.descend_mut(&["x", "y"], 0);

        // Assert — same length bucket, no children under its
        // root (no prefix-token branching), both lines land at
        // the length-bucket root itself.
        let length_2 = tree.by_length.get(&2).expect("length-2 bucket present");
        assert!(length_2.root.children.is_empty());
    }

    #[test]
    #[should_panic(expected = "must be non-empty")]
    fn descend_mut_panics_on_empty_input() {
        // Arrange
        let mut tree = Tree::new();
        let masked: [&str; 0] = [];

        // Act + Assert
        let _ = tree.descend_mut(&masked, DEFAULT_PREFIX_DEPTH);
    }

    // OwnedToken / Token round-trip

    #[test]
    fn owned_token_as_borrowed_round_trips_fixed() {
        // Arrange
        let owned = OwnedToken::Fixed("user".to_string());

        // Act
        let borrowed = owned.as_borrowed();

        // Assert
        assert_eq!(borrowed, Token::Fixed("user"));
    }

    #[test]
    fn owned_token_as_borrowed_round_trips_wildcard() {
        // Arrange
        let owned = OwnedToken::Wildcard;

        // Act
        let borrowed = owned.as_borrowed();

        // Assert
        assert_eq!(borrowed, Token::Wildcard);
    }

    #[test]
    fn owned_template_feeds_sim_seq_via_borrowed_view() {
        // Arrange — one OwnedToken-stored template, one borrowed
        // candidate line; sim_seq must accept the borrowed view
        // unchanged.
        use crate::sim_seq::sim_seq;
        let template = [
            OwnedToken::Fixed("user".to_string()),
            OwnedToken::Wildcard,
            OwnedToken::Fixed("logged".to_string()),
        ];
        let line = ["user", "42", "logged"];

        // Act
        let borrowed: Vec<Token<'_>> = template.iter().map(OwnedToken::as_borrowed).collect();
        let r = sim_seq(&line, &borrowed);

        // Assert — every position matches (literal, wildcard,
        // literal); ratio = 3/3 = 1.0.
        assert!(
            (r - 1.0).abs() < f32::EPSILON,
            "borrowed view of stored template must yield 1.0, got {r}",
        );
    }
}
