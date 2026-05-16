//! Drain prefix tree (RFC 0001 §6.2 step 3).
//!
//! Lays out the parent path that
//! [`crate::cluster::MinerCluster::ingest`] walks before per-leaf
//! candidate selection. The shape is the Drain-paper canonical
//! one: root → length-N node → prefix-token nodes → leaf list.
//!
//! # Scope of this module
//!
//! The tree backs [`crate::cluster::MinerCluster`]'s per-tenant
//! template store. Leaves carry the §6.1 template-key fields
//! (template tokens, `template_id`, `template_version`,
//! `severity_number`, `scope_name`); stored templates may contain
//! [`OwnedToken::Wildcard`] positions introduced by §6.2 step 5
//! widening.
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
/// Carries the [`OwnedToken`] template, its `template_id`, its
/// `template_version`, and the `(severity_number, scope_name)`
/// half of the §6.1 *Template-key composition* tuple — without
/// those two fields, two records that share masked tokens but
/// differ in severity or scope would silently coalesce, violating
/// H1.4 / H1.5.
///
/// `template_version` starts at `1` on fresh-leaf creation and
/// increments on every widening or type-expansion per RFC 0001
/// §6.4. Versioning lives on the leaf rather than as a sibling map
/// because every widening is decided in the same scope that mutates
/// the leaf's template — the version stamp is part of the same
/// invariant. The audit event records `(old_version, new_version)`
/// from the same bump.
///
/// `slot_types` and retained-body counts (the rest of the §6.1
/// leaf payload) will be added when the type-expansion and
/// body-retention PRs land.
#[derive(Debug, Clone)]
pub struct Leaf {
    pub template: Vec<OwnedToken>,
    /// Cluster-wide unique identifier per RFC 0001 §6.1 — the
    /// `template_id` allocated by [`crate::cluster::MinerCluster`]
    /// when the leaf was first created.
    pub template_id: u64,
    /// Monotonic version stamp per RFC 0001 §6.4. Starts at `1`
    /// on fresh-leaf creation; the cluster bumps it by one on each
    /// widening or type-expansion and records the bump in an
    /// audit event. Clean attaches (no widening) do not bump it.
    pub template_version: u32,
    /// `LogRecord.severity_number` half of the template key per
    /// RFC 0001 §6.1 *Template-key composition*. `0` =
    /// `UNSPECIFIED` is its own bucket (RFC0001.11), distinct from
    /// any specified severity.
    pub severity_number: u8,
    /// `InstrumentationScope.name` half of the template key. `None`
    /// is its own bucket (RFC0001.11), distinct from any specified
    /// scope.
    pub scope_name: Option<String>,
}

/// Internal node at a prefix-token level (or the per-length root).
///
/// `children` is keyed on the masked token at this level. `leaves`
/// holds the candidate set at the deepest prefix level reached
/// by [`Tree::descend_mut`]. By **convention** intermediate nodes
/// carry an empty `leaves` — the type does not enforce this; the
/// field is `pub` so [`crate::cluster::MinerCluster::ingest`] can
/// push into the node `descend_mut` returns. Pushing into an
/// intermediate node would be a caller bug.
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

/// Root of the Drain prefix tree. One per tenant via
/// [`crate::cluster::MinerCluster`]; this module stays
/// tenant-agnostic.
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
    /// runs [`crate::sim_seq::sim_seq`] over. When
    /// `masked.len() < prefix_depth` the walk stops early — the
    /// entire line is consumed as path, so short lines bottom
    /// out at a **shallower** prefix level than long ones. This
    /// matches the Drain paper.
    ///
    /// # Panics
    ///
    /// If `masked` is empty. The miner's tokenize → mask path
    /// guarantees at least one token before any tree call; an
    /// empty input here means a caller bypassed that path.
    pub fn descend_mut(&mut self, masked: &[&str], prefix_depth: usize) -> &mut PrefixNode {
        assert!(
            !masked.is_empty(),
            "descend_mut precondition: masked must be non-empty",
        );

        let length = masked.len();
        let length_node = self.by_length.entry(length).or_default();

        // Walk only as many levels as the line has tokens —
        // shorter lines bottom out before exhausting prefix_depth.
        let walk_depth = prefix_depth.min(length);
        let path = &masked[..walk_depth];

        descend_recursively(&mut length_node.root, path)
    }

    /// Read-only counterpart to [`Tree::descend_mut`]. Returns the
    /// [`PrefixNode`] that *would* own the leaf list for `masked` if
    /// every node along the path already exists, or [`None`] if any
    /// node in the path is missing.
    ///
    /// Used by [`crate::cluster::MinerCluster::ingest`] for the
    /// candidate-selection phase — finding the best leaf to attach
    /// to (or widen) before committing the `template_id` allocation
    /// that [`Tree::descend_mut`] would entail.
    ///
    /// # Panics
    ///
    /// If `masked` is empty (same precondition as [`Tree::descend_mut`]).
    #[must_use]
    pub fn descend(&self, masked: &[&str], prefix_depth: usize) -> Option<&PrefixNode> {
        assert!(
            !masked.is_empty(),
            "descend precondition: masked must be non-empty",
        );

        let length = masked.len();
        let length_node = self.by_length.get(&length)?;

        let walk_depth = prefix_depth.min(length);
        let path = &masked[..walk_depth];

        descend_immutable(&length_node.root, path)
    }

    /// Total leaf count across the tree. Suitable for the
    /// `template_count` metric (every leaf corresponds to exactly
    /// one template). Not on the ingest hot path — full traversal.
    #[must_use]
    pub fn leaf_count(&self) -> usize {
        self.by_length
            .values()
            .map(|ln| count_leaves(&ln.root))
            .sum()
    }

    /// Collect references to every [`Leaf`] in the tree. Order is
    /// not guaranteed (`HashMap` iteration). For introspection
    /// (`templates_for`); not for the ingest hot path.
    #[must_use]
    pub fn collect_leaves(&self) -> Vec<&Leaf> {
        let mut out = Vec::new();
        for length_node in self.by_length.values() {
            collect_leaves_recursive(&length_node.root, &mut out);
        }
        out
    }
}

fn descend_immutable<'t>(node: &'t PrefixNode, path: &[&str]) -> Option<&'t PrefixNode> {
    if path.is_empty() {
        return Some(node);
    }
    let (head, tail) = path.split_first().expect("non-empty by guard above");
    let child = node.children.get(*head)?;
    descend_immutable(child, tail)
}

fn count_leaves(node: &PrefixNode) -> usize {
    node.leaves.len() + node.children.values().map(count_leaves).sum::<usize>()
}

fn collect_leaves_recursive<'t>(node: &'t PrefixNode, out: &mut Vec<&'t Leaf>) {
    out.extend(node.leaves.iter());
    for child in node.children.values() {
        collect_leaves_recursive(child, out);
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
                template_version: 1,
                severity_number: 0,
                scope_name: None,
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

    // Tree::descend (immutable)

    #[test]
    fn descend_returns_none_when_length_bucket_missing() {
        // Arrange — empty tree, descend asks for a length never seen.
        let tree = Tree::new();

        // Act
        let r = tree.descend(&["a", "b"], DEFAULT_PREFIX_DEPTH);

        // Assert
        assert!(r.is_none());
    }

    #[test]
    fn descend_returns_none_when_prefix_path_missing() {
        // Arrange — same length as a known path but a different
        // first-prefix token. The length bucket exists; the
        // prefix child does not.
        let mut tree = Tree::new();
        let _ = tree.descend_mut(&["user", "42", "logged"], DEFAULT_PREFIX_DEPTH);

        // Act — same length 3, different first-prefix token.
        let r = tree.descend(&["GET", "/home", "200"], DEFAULT_PREFIX_DEPTH);

        // Assert
        assert!(r.is_none());
    }

    #[test]
    fn descend_returns_some_at_exact_path_descend_mut_built() {
        // Arrange — materialise a path, then look it up.
        let mut tree = Tree::new();
        {
            let parent = tree.descend_mut(&["user", "42", "logged"], DEFAULT_PREFIX_DEPTH);
            parent.leaves.push(Leaf {
                template: [
                    OwnedToken::Fixed("user".to_string()),
                    OwnedToken::Fixed("42".to_string()),
                    OwnedToken::Fixed("logged".to_string()),
                ]
                .into(),
                template_id: 7,
                template_version: 1,
                severity_number: 0,
                scope_name: None,
            });
        }

        // Act
        let parent = tree
            .descend(&["user", "42", "logged"], DEFAULT_PREFIX_DEPTH)
            .expect("path was built");

        // Assert — same leaf observable via immutable descend.
        assert_eq!(parent.leaves.len(), 1);
        assert_eq!(parent.leaves[0].template_id, 7);
    }

    #[test]
    #[should_panic(expected = "must be non-empty")]
    fn descend_panics_on_empty_input() {
        // Arrange
        let tree = Tree::new();
        let masked: [&str; 0] = [];

        // Act + Assert
        let _ = tree.descend(&masked, DEFAULT_PREFIX_DEPTH);
    }

    // leaf_count + collect_leaves

    #[test]
    fn leaf_count_is_zero_for_empty_tree() {
        // Arrange
        let tree = Tree::new();

        // Act
        let n = tree.leaf_count();

        // Assert
        assert_eq!(n, 0);
    }

    #[test]
    fn leaf_count_sums_across_length_buckets_and_prefix_paths() {
        // Arrange — three leaves across two length buckets:
        //   - length 2, prefix "GET" — 1 leaf
        //   - length 3, prefix "user 42" — 2 leaves (same parent)
        let mut tree = Tree::new();
        {
            let p = tree.descend_mut(&["GET", "/home"], DEFAULT_PREFIX_DEPTH);
            p.leaves.push(Leaf {
                template: [
                    OwnedToken::Fixed("GET".to_string()),
                    OwnedToken::Fixed("/home".to_string()),
                ]
                .into(),
                template_id: 1,
                template_version: 1,
                severity_number: 0,
                scope_name: None,
            });
        }
        {
            let p = tree.descend_mut(&["user", "42", "in"], DEFAULT_PREFIX_DEPTH);
            p.leaves.push(Leaf {
                template: [
                    OwnedToken::Fixed("user".to_string()),
                    OwnedToken::Fixed("42".to_string()),
                    OwnedToken::Fixed("in".to_string()),
                ]
                .into(),
                template_id: 2,
                template_version: 1,
                severity_number: 0,
                scope_name: None,
            });
            p.leaves.push(Leaf {
                template: [
                    OwnedToken::Fixed("user".to_string()),
                    OwnedToken::Fixed("42".to_string()),
                    OwnedToken::Fixed("out".to_string()),
                ]
                .into(),
                template_id: 3,
                template_version: 1,
                severity_number: 0,
                scope_name: None,
            });
        }

        // Act
        let n = tree.leaf_count();

        // Assert
        assert_eq!(n, 3);
    }

    #[test]
    fn collect_leaves_returns_every_leaf_ignoring_order() {
        // Arrange — two leaves in different length buckets.
        let mut tree = Tree::new();
        {
            let p = tree.descend_mut(&["a", "b"], DEFAULT_PREFIX_DEPTH);
            p.leaves.push(Leaf {
                template: [
                    OwnedToken::Fixed("a".to_string()),
                    OwnedToken::Fixed("b".to_string()),
                ]
                .into(),
                template_id: 10,
                template_version: 1,
                severity_number: 0,
                scope_name: None,
            });
        }
        {
            let p = tree.descend_mut(&["x", "y", "z"], DEFAULT_PREFIX_DEPTH);
            p.leaves.push(Leaf {
                template: [
                    OwnedToken::Fixed("x".to_string()),
                    OwnedToken::Fixed("y".to_string()),
                    OwnedToken::Fixed("z".to_string()),
                ]
                .into(),
                template_id: 20,
                template_version: 1,
                severity_number: 0,
                scope_name: None,
            });
        }

        // Act
        let leaves = tree.collect_leaves();

        // Assert — set semantics (HashMap iteration unordered).
        let ids: std::collections::HashSet<u64> = leaves.iter().map(|l| l.template_id).collect();
        assert_eq!(ids, std::collections::HashSet::from([10, 20]));
    }
}
