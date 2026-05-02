# Verification

> **Status:** active. This document is the process spec. The proposed
> amendments to `docs/rfcs/README.md` and `CLAUDE.md` at the bottom of
> this file are tracked separately and applied in their own PR once
> the structure here is settled.

## What this doc is for

The Ourios docs already define invariants (`CLAUDE.md` §3), hazards
(`docs/hazards.md`), per-RFC testing strategies (RFC §6 — see §2.5),
thesis-gates (`docs/benchmarks.md`), and the project's testing
discipline (`CLAUDE.md` §6.2). What is missing is the **described
process** that connects them: how a contributor (human or agent) takes
a §3 invariant or an H-x hazard, turns it into reviewable acceptance
criteria, turns those into red tests, drives them green, and gates an
RFC's transition to `accepted`.

This doc fills that gap. It is human-readable process, not test code,
not tooling, not a coverage policy.

---

## 1. The flow

Six links, four gates between them. The diagram below names them; the
text after walks the chain in the order a contributor encounters it.

```
Invariant (§3)        Hazard (H-x)        RFC (§§1–4)
       \                 |                   /
        \________________|__________________/
                         ↓
                Acceptance criteria
              (RFC §5 — normative,
               structured prose)
                         ↓
                   Red tests
                  (compile, fail)
                         ↓
                  Green tests
              (unit + property + corpus)
                         ↓
                  Validated
        (corpus + thesis-gates pass on
         representative inputs)
```

The chain has two entry points (`Invariant`, `Hazard`) that converge
on the third (`RFC`). An RFC enumerates the invariants and hazards it
touches in its §1 *Summary*; reviewers verify the enumeration is
exhaustive at the *Drafted → Specified* gate.

**Invariant → RFC.** A `CLAUDE.md` §3 invariant is a project-level
promise. Until an RFC operationalises it, the invariant is a known
debt. §4 *Entry points* describes the three doors into this chain.

**Hazard → RFC.** Each `hazards.md` H-x item names the RFCs and crates
responsible in its *Mitigation* and *See also* fields. The hazard does
not move; the RFC inherits the obligation to defend it.

**RFC → Acceptance criteria.** Acceptance criteria live in RFC §5
(see §2) and translate the invariants and hazards the RFC touches into
testable scenarios. The *Specified* gate ratifies the list.

**Acceptance criteria → Red tests.** A red test is a compiling stub
that fails — typically with `todo!()` or `unimplemented!()` — and
references the scenario id in a doc comment. Red tests are *not*
required at the *Specified* gate; they are the artefact of crossing
the *Red* gate, immediately before implementation begins. Forcing
stubs to compile at *Specified* would push authors into premature
specificity about types and signatures. Red stubs are tagged
`#[ignore]` so the *outer* CI loop stays green while the *inner*
loop (the implementor running `cargo test -- --ignored` locally)
sees the `todo!()`s fire as a TODO list — see §3 for the two-loop
spec.

**Red tests → Green tests.** Implementation lands; each stub becomes a
real test that passes; unit, property, and corpus tests cover the
scenario as `CLAUDE.md` §6.2 dictates. The *Green* gate confirms every
§5 acceptance criterion has a matching passing test.

**Green tests → Validated.** The thesis-gates in `benchmarks.md` §7
that the RFC's pillars touch must pass on representative corpora. Once
they do, the RFC's `status:` flips to `validated`. Maintainer sign-off
then flips it to `accepted`.

## 2. Acceptance criteria

The contract here is single-typed: every invariant or hazard the RFC
touches resolves to one or more scenarios, each with an id, a leading
clause grammar, and a greppable counterpart in test code.

### 2.1 Format

Structured prose using bold leading clauses. Each scenario carries a
short numeric id (see §2.2) and follows the `Given / When / Then /
And` pattern:

> **Scenario H1.1 — Semantically distinct templates do not silently merge**
> - **Given** a corpus containing `user logged in <*>` and
>   `user logged out <*>`
> - **When** similarity threshold is 0.7 (default)
> - **Then** the two remain distinct `template_id`s
> - **And** any widening produces an audit event recording both old
>   and new templates

The format is the markdown the project already uses, not Gherkin. We
do not adopt `.feature` files, cucumber-rs, or any other BDD tooling:
the test code is Rust (`CLAUDE.md` §6.2), and the scenario lives in
the RFC where reviewers are already reading. A second source of truth
— a `.feature` file checked separately — would drift, and the tooling
does not pay for itself at our scale.

### 2.2 Scenario ids

Three id grammars, chosen to make the source of the obligation visible
at a glance:

- `H<n>.<m>` — hazard-rooted; `H1.1` is the first scenario defending
  hazard H1.
- `§3.<n>.<m>` — invariant-rooted; `§3.4.2` is the second scenario
  defending `CLAUDE.md` §3.4 (WAL-before-ack).
- `RFC<NNNN>.<m>` — RFC-internal; reserved for scenarios that defend
  an RFC's own design decisions, not a numbered invariant or hazard.
  Example: `RFC0001.3` for a Drain3-extension behaviour that is not
  load-bearing for any §3 invariant but is part of the RFC's contract.

Numbers within an id family are assigned in the order scenarios are
written and never renumbered. A retired scenario keeps its number;
new scenarios append. This gives `git log -S "H1.1"` a stable target
across the lifetime of the project.

### 2.3 Greppability

The id is referenced from the test code in a doc comment, exactly:

```rust
/// Scenario H1.1 — Semantically distinct templates do not silently merge.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
fn login_and_logout_do_not_merge_at_default_threshold() { /* … */ }
```

`grep -R "H1.1" .` then yields the scenario in the RFC, the test in
the crate, and any cross-references in the docs — bidirectional in
one command. If a scenario is renamed, both ends move in the same
commit.

### 2.4 Normative vs. exhaustive

Acceptance criteria are the *normative* tests an RFC promises will
exist. The implementation will write many more — regression tests,
edge cases, performance smokes — and those are not catalogued in the
RFC. Reviewers ratify the normative set: every invariant and hazard
the RFC touches has at least one scenario, and the scenarios as
written are testable in principle.

The opposite mistake — listing every test the implementation will
ever write — turns the RFC into a test plan and freezes the
implementation. We do not do that.

### 2.5 Location in the RFC

Acceptance criteria are a new RFC §5, immediately before *Testing
strategy*. The placement is deliberate: criteria are the spec the
testing strategy operationalises, so reviewers reading the RFC top
to bottom encounter the *what* before the *how*. The proposed
amendment to `docs/rfcs/README.md` at the bottom of this file
captures the renumbering: existing §5 *Testing strategy* shifts to
§6, *Open questions* to §7, *References* to §8.

## 3. The RFC maturity model

Five stages, four gates. Each stage is a value of the RFC's
`status:` frontmatter field, so an RFC's current maturity is visible
without reading the body:

| Stage | What exists | Gate to next |
|---|---|---|
| **Drafted** | RFC §§1–4 and §§7–8 filled; §§5–6 may be stubbed | Peer review of design |
| **Specified** | §5 acceptance criteria written, scenarios numbered | Review: do the criteria cover every invariant and hazard the RFC touches? Are they testable *in principle*? |
| **Red** | Test stubs compile, are tagged `#[ignore]`, and fail with `todo!()` (or equivalent) when run | Implementation begins |
| **Green** | All §5 criteria pass; unit + property + corpus tests green | Validation against representative inputs |
| **Validated** | Thesis-gates in `benchmarks.md` §7 pass on representative corpora | Maintainer signs off; status flips to `accepted` |

`accepted` is a distinct terminal status — it represents maintainer
sign-off after `Validated` is reached. `rejected` and `superseded` are
the other terminals, all three reachable from anywhere in the maturity
ladder. A *Drafted* or *Specified* RFC may be rejected on review
without ever reaching *Red*; an *Accepted* RFC may be superseded by a
later one without re-traversing the chain.

The table is the spec; the paragraphs below explain what artefacts
exist at each stage and what a reviewer is ratifying.

**Drafted.** The RFC has §§1–4 (Summary, Motivation, Proposed design,
Alternatives considered) plus §§7–8 (Open questions, References)
filled enough that two engineers reading it would produce roughly the
same implementation. *Acceptance criteria* (§5) and *Testing strategy*
(§6) may be empty or stubbed. The PR is open with `status: drafted`;
review focuses on whether the design is correct in principle. The
gate to *Specified* is a peer reviewer saying "yes, this design is
what we want — now write down the contract."

**Specified.** §5 *Acceptance criteria* is filled. Every invariant in
`CLAUDE.md` §3 and every hazard in `hazards.md` that the RFC touches
has at least one numbered scenario. §6 *Testing strategy* references
those scenarios and names the technique (`proptest`, corpus,
`criterion`) for each. The reviewer asks one question: *could a
competent implementor turn each criterion into a test as written?* If
the answer is no — for any criterion — the RFC has a gap and goes
back to *Drafted*.

**The Specified gate is the most valuable.** It is the only gate
where the cost of being wrong is bounded by review time rather than
implementation time. We do not require test stubs to compile here;
forcing stubs would push authors into premature decisions about
function signatures, traits, and module structure, which is the *Red*
gate's job, not this one.

**Red.** Test stubs exist, are tagged `#[ignore]`, and fail when
run. Each stub carries a doc comment naming its scenario id
(§2.3). Stubs may be `todo!()`, `unimplemented!()`, `assert!(false)`
— anything that compiles and fails. Implementation may begin.

The Red signal lives at two granularities, deliberately:

- *Inner loop (local dev cycle).* The implementor working on a
  stub runs `cargo test <name> -- --ignored` and watches the
  `todo!()` panic. Each panic is one TODO item; as the body fills
  in, the `#[ignore]` comes off and the test joins the default
  run.
- *Outer loop (CI).* Default `cargo test` skips ignored tests,
  so the Red-stage PR lands cleanly through branch protection
  rather than fighting it. CI's signal that the Red gate is
  satisfied is structural: stubs compile, every §5 scenario has
  an `#[ignore]`'d test with a matching id, and `cargo test
  --include-ignored` exits non-zero on each. (The greppability
  contract in §2.3 makes the per-scenario coverage check
  mechanical — `grep -R "H1.1"` returning both the RFC line and
  the test stub line is the assertion.)

The two-loop split is what lets us treat the *Red* status as a
landable, mergeable state rather than a half-broken branch. A
Red-stage main is healthy: outer loop green, inner loop fully
populated with the work that needs doing.

The gate is mechanical: every scenario in §5 has at least one
stub with a matching id, the stub is tagged `#[ignore]`, and
`cargo test --include-ignored` exits non-zero on each.

**Green.** Implementation lands. Every stub becomes a real test;
unit, property, and corpus tests cover their scenarios as
`CLAUDE.md` §6.2 dictates. `cargo test --all-features` passes. The
reviewer confirms each §5 criterion now resolves to a passing test
(the greppability contract makes this mechanical). No performance
claim is made yet.

**Validated.** Every thesis-gate in `benchmarks.md` §7 *that the
RFC's pillars touch* passes on representative corpora. Maintainer
inspects the corpus and the delta against target, signs off, and
flips `status:` to `accepted`. The RFC is now binding; subsequent
changes go through the regression handling in §3.1.

### 3.1 Regression handling after `Validated`

A failing test on a previously-`Validated` RFC is, by default, *the
test doing its job*. The RFC does not reopen. Standard PR workflow:
fix the regression, ship the patch, the test stays green.

The RFC reopens only when a single criterion fails *repeatedly* on
the same code path — concretely, when the same scenario id fails on
three independent commits within a 30-day rolling window, or when
two distinct regressions touch the same criterion within the same
window. The threshold is that the criterion has stopped being a
defence and has become a moving target; that is a signal the RFC's
commitment is under-defended or under-specified, and the design (not
just the implementation) needs revisiting.

This threshold is informal at the Specified gate; it sharpens once
real signals exist. The point of writing it down now is that
contributors do not race to reopen RFCs on every CI flake, nor pretend
a repeated structural failure is just bad luck.

Thesis-gate failures during `Validated` follow `benchmarks.md` §7's
existing escalation rule (one fail on one corpus → tuning RFC; two or
more → pillar RFC, pause), not this section.

### 3.2 Outer loop vs. inner loop

The maturity model is the **outer loop**. Each stage names a
checkpoint that an external reviewer can verify: at *Specified* the
scenarios are written, at *Red* the stubs compile and fail, at *Green*
the same stubs pass. Nothing in the outer loop says how a developer
fills the *Red → Green* transition.

The recommended **inner loop** is classic Beck-style TDD: write one
failing test, make it pass with minimal code, refactor, triangulate
by writing the next test that forces generalisation, repeat. It is
not mandatory — a developer who prefers to stub all scenarios up
front and implement against them is welcome to. The outer loop only
requires that every §5 scenario has a stub by the *Red* gate and a
passing test by the *Green* gate.

Two consequences worth being explicit about:

1. **More tests than scenarios.** The inner loop typically writes
   many tests per scenario — one per concrete example, then
   regression tests as bugs surface. Acceptance criteria (§2.4) are
   the *normative* set the RFC is held to; the inner loop fills out
   the rest.
2. **No `refactor` stage in the model.** Refactoring is part of the
   inner loop, not a maturity stage. A *Green* or *Validated* RFC
   may be refactored without re-traversing the chain, as long as
   every §5 criterion stays green.

The split is the BDD/ATDD outer-shell convention adapted to a project
already committed to Rust, `proptest`, and `criterion`: the scenarios
are written in the BDD-flavoured prose of §2.1 because they live in
RFCs and humans read them; the tests are written in the TDD-flavoured
loop developers already know.

## 4. Entry points

The same machinery, three doors:

- **Invariant entry** — an item in `CLAUDE.md` §3. The criteria live
  in the RFC that operationalises that invariant; if no RFC yet exists
  for the relevant subsystem, the invariant is a known debt and the
  next RFC for that subsystem must address it.
- **Hazard entry** — an item in `hazards.md`. Each hazard's
  *Mitigation* section names the RFCs and crates responsible; their
  acceptance criteria must reference the hazard id.
- **RFC entry** — a new RFC under `docs/rfcs/`. The RFC enumerates the
  invariants and hazards it touches in its §1 Summary; criteria in §5
  must cover each.

## 5. Relationship to `benchmarks.md`

Correctness gates live here. Thesis-gates live in `benchmarks.md` §7.
An RFC reaches `Validated` only when **both**:

- Every §5 acceptance criterion has a passing test, **and**
- Every thesis-gate in `benchmarks.md` §7 *that the RFC's pillars
  touch* passes on representative corpora.

Single sentence; intentional non-duplication. `benchmarks.md` stays
the performance owner.

## 6. Worked example

A concrete trace of the chain in §1, against an artefact that already
exists. RFC 0001 *Template miner* is currently `status: draft`
(becoming `drafted` once the amendment to `docs/rfcs/README.md`
lands). Its operationalisation of `CLAUDE.md` §3.1 *No silent template
merges* and `hazards.md` H1 *Template miner correctness* is the first
place this process gets to bite on real material.

### 6.1 Invariant → RFC

`CLAUDE.md` §3.1 promises:

> A template merge that crosses semantic boundaries (e.g. merging
> "user logged in" with "user logged out" because they share token
> structure) corrupts the backend.

`hazards.md` H1 names the canonical horror — `user logged in <*>` and
`user logged out <*>` differing in one token, merging under a
permissive threshold to `user logged <*> <*>`, a query for the login
event silently returning logout rows.

RFC 0001 §6.4 *Merge policy* is the section that defends the
invariant. As of the *Drafted* gate it commits to "When two templates
become candidates for merge", an audit event schema, and the rule
"Default: strict. Never silent. No exceptions."

### 6.2 RFC → Acceptance criteria

The *Specified* gate adds a new §5 to RFC 0001:

> **Scenario H1.1 — Semantically distinct templates do not silently merge**
> - **Given** a corpus containing `user logged in <*>` and
>   `user logged out <*>`
> - **When** similarity threshold is 0.7 (default)
> - **Then** the two remain distinct `template_id`s
> - **And** any widening produces an audit event recording both old
>   and new templates
>
> **Scenario H1.2 — Lossy match retains body**
> - **Given** a line whose best match has confidence in the lossy
>   zone (`floor ≤ x < threshold`)
> - **When** the line is ingested
> - **Then** the `body` column contains the original line bytes
> - **And** the row carries `lossy_flag = false`
>   (`lossy_flag` is reserved for tokenizer / preprocessing failure
>   per `docs/rfcs/0001-template-miner.md` §6.6 — the lossy
>   *zone* retains the body but reconstruction still succeeds)
>
> **Scenario H1.3 — Every merge emits an audit event**
> - **Given** any sequence of inputs that triggers a template
>   widening
> - **When** the merge completes
> - **Then** an audit event exists naming the old template, the new
>   template, the tenant id, the timestamp, and the reason

Three scenarios cover §3.1's three rules: do not merge across
semantics, retain bodies on low confidence, audit every merge.
Reviewers ratify that this is exhaustive against `CLAUDE.md` §3.1 and
H1; they do not catalogue every edge-case test the implementation
will write.

### 6.3 Acceptance criteria → Red tests

The *Red* gate adds three stubs to `crates/ourios-miner/tests/`:

```rust
/// Scenario H1.1 — Semantically distinct templates do not silently merge.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
#[ignore = "RFC 0001 Red gate — implementation pending"]
fn h1_1_login_and_logout_remain_distinct_at_default_threshold() {
    todo!("RFC 0001 §6.4");
}

/// Scenario H1.2 — Lossy match retains body.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
#[ignore = "RFC 0001 Red gate — implementation pending"]
fn h1_2_lossy_match_retains_body() {
    todo!("RFC 0001 §6.6");
}

/// Scenario H1.3 — Every merge emits an audit event.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
#[ignore = "RFC 0001 Red gate — implementation pending"]
fn h1_3_every_merge_emits_an_audit_event() {
    todo!("RFC 0001 §6.4");
}
```

Default `cargo test` skips the ignored stubs and passes (outer
loop / CI green); `cargo test -- --ignored` exits non-zero with
all three failing (inner loop / Red signal). The gate is
satisfied; implementation may begin.

### 6.4 Red → Green

Implementation lands across `ourios-miner` (and supporting types in
`ourios-core`). The three stubs become real tests: H1.1 ingests the
two-template corpus, asserts two distinct `template_id`s, and queries
the audit log for absence of merge events. H1.2 ingests a line whose
token similarity falls in the lossy zone and asserts that the
row's `body` carries the original bytes and `lossy_flag` is
`false` (the flag is reserved for the H7 reconstruction-failure
case; see RFC 0001 §6.6). H1.3 ingests a sequence that provokes a
widening and asserts the audit event's structure.

`cargo test --all-features` passes. Reviewers confirm each H1.x id
now resolves to a passing test via grep. No benchmark claim is made.

### 6.5 Green → Validated

`benchmarks.md` C2 *Template count convergence* is the thesis-gate
that H1 most directly touches: if the miner is silently merging
across semantics, template count grows wrong. The benchmark harness
runs C2 on the LogPAI corpora and any self-collected corpus
available, plots template count vs. lines ingested, and asserts the
convergence target.

Once C2 passes — and any other thesis-gate the RFC's pillars touch —
the maintainer signs off. RFC 0001's `status:` flips to `accepted`.
The miner's contract is now binding.

### 6.6 The failure mode that re-opens the RFC

A hypothetical: six months in, three independent PRs land that each
add a workaround to keep H1.1 green — a special-case for common verb
pairs, then for HTTP method tokens, then for log-level tokens. Each
workaround is small, each test stays green. By the fourth PR, a
reviewer notices: the *criterion* has stopped being a defence and has
become a moving target. Per §3.1, the RFC reopens. The right answer
is not a fifth workaround; it is to revisit RFC 0001 §6.4 — the merge
policy itself is under-specified for the workloads we are seeing.

This is what the threshold in §3.1 is for. It is not a CI-flake
counter; it is a signal that the design's defence has eroded and
needs to be redrawn before more code is written on top of it.

## 7. What this doc is *not*

- Not test-tooling guidance — `proptest`, `criterion`, etc. live in
  `CLAUDE.md` §6.2.
- Not a coverage policy — Ourios is a correctness project; line
  coverage is the wrong metric.
- Not an agent-instruction file — agents follow it because it is
  written down, not because it speaks to them.

## 8. Resolved decisions

Three questions raised during the outline review, decided before
expansion so the rationale is preserved:

- **Maturity stages appear in RFC frontmatter** as the `status:` field.
  Reviewers and tooling see an RFC's current stage without reading the
  body. See §3.
- **Single regressions do not reopen a Validated RFC.** A failing test
  on an existing criterion is the test doing its job; standard PR
  workflow applies. Repeated regression on the same criterion (rough
  threshold: same scenario id failing on three independent commits, or
  two distinct regressions touching the same criterion, both measured
  in a 30-day rolling window) signals the criterion has stopped being
  a defence and reopens the RFC. See §3.1.
- **Thesis-gate failures during Validated follow `benchmarks.md` §7**,
  not this doc. One thesis-gate failing on one corpus → tuning RFC;
  two or more → pillar RFC and an implementation pause.

---

## Proposed amendment — `docs/rfcs/README.md`

Two changes. Shown as the new text:

### In *Required frontmatter*

Update the `status` field's valid values from the current
four-state list to the five-stage maturity model plus terminals:

> ```yaml
> status: drafted | specified | red | green | validated | accepted | rejected | superseded
> ```

The maturity stages (`drafted` through `validated`) are gates an RFC
moves through; `accepted` is the terminal post-maintainer-signoff
binding state; `rejected` and `superseded` are the off-ramps. See
`docs/verification.md` §3.

### In *Required sections*

Insert a new item between the current §4 *Alternatives considered*
and §5 *Testing strategy*, renumbering subsequent items:

> 5. **Acceptance criteria** — normative scenarios, one per invariant
>    or hazard the RFC touches. Format: structured prose with
>    `Given / When / Then / And` leading clauses; each scenario
>    carries an id of the form `H1.1`, `§3.4.2`, or `RFC<NNNN>.<m>`,
>    referenced from the test code so the mapping is greppable. See
>    `docs/verification.md` §2.

*Testing strategy* shifts to §6, *Open questions* to §7, and
*References* to §8.

### In *Lifecycle*

Replace the current four-status list with the five-stage maturity
model:

> 1. **Drafted** — PR opened with status `drafted`. Sections §§1–4
>    and §§7–8 are filled. Discussion happens in PR review.
> 2. **Specified** — §5 acceptance criteria are written, every
>    invariant and hazard the RFC touches has at least one scenario,
>    and review has confirmed the criteria are testable in
>    principle.
> 3. **Red** — test stubs exist and fail. Implementation may begin.
> 4. **Green** — all acceptance criteria pass; unit + property +
>    corpus tests green.
> 5. **Validated** — thesis-gates in `docs/benchmarks.md` §7 pass on
>    representative corpora. Maintainer flips status to `accepted`.
>
> A regression detected after `Validated` either reopens the RFC (if
> a criterion is invalidated) or spawns a tuning RFC per
> `benchmarks.md` §7 (if a thesis-gate degrades). See
> `docs/verification.md` §3.

The earlier `superseded` and `rejected` entries remain unchanged.

### Existing RFC frontmatter

RFC 0001 and RFC 0002 currently carry `status: draft`. The amendment
PR renames both to `status: drafted` so the maturity model applies
uniformly. No content change to the RFCs themselves at that step.

---

## Proposed amendment — `CLAUDE.md`

A single new subsection under §5 *Development workflow*, following
§5.5 *One-word mode*:

> ### 5.6 Verification process
>
> The path from invariant or hazard to passing test is described in
> `docs/verification.md`. Acceptance criteria live in RFC §5;
> `docs/rfcs/README.md` defines the maturity stages an RFC moves
> through. The shortest version of the rule: *if a criterion cannot
> be turned into a test, the RFC has a gap.*

No change to §6.2 *Testing discipline*; verification.md links to it.
The §6.2 content (proptest, corpus tests, crash recovery, criterion)
is the *catalogue of techniques*; verification.md is the *process
that decides which technique is required where*.

---

## Applying the amendments

The body of this document is the verification process spec. The two
proposed amendments above are pending application:

- `docs/rfcs/README.md` — `status:` value list, new §5 *Acceptance
  criteria* in *Required sections* with renumbering, lifecycle
  rewrite, `draft` → `drafted` rename in RFC 0001 and 0002.
- `CLAUDE.md` — new §5.6 *Verification process*.

Both should land in a single PR. RFC 0001 then gets a §5 *Acceptance
criteria* applied as the first concrete use of the process — the
worked example in §6 of this document is the target shape, and
applying it will probably surface specificity gaps in RFC 0001's
existing design. That surfacing is the point.

Add this document to `docs/SUMMARY.md` under the *Architecture*
header in the same PR that applies the amendments.
