---
title: "Template mining in Ourios: what Drain says, what it leaves out, and what we commit to"
speaker: Jens Holdgaard Pedersen
drafting-assistance: Claude
target-duration: 45 minutes
audience: engineers familiar with log backends but not the Drain paper
companion-rfc: docs/rfcs/0001-template-miner.md
created: 2026-04-24
---

# Template mining in Ourios

*A lecture manuscript. Prose is written for spoken delivery; figures
are sized to lift onto slides.*

---

## Abstract

Log storage at scale has a compression problem that looks unsolvable
when you squint at it. A terabyte of raw log lines is mostly
repetition — the same twenty-odd templates interleaved with
ever-changing parameters — but commodity byte-level compressors like
zstd cannot see that structure. They see bytes. Template mining is the
layer that turns the repetition into a first-class citizen before any
byte codec runs, and the algorithm we use — Drain, published in 2017 —
is so simple it fits on one slide.

But the paper is ten pages long, and a production log backend needs
answers to at least six questions the paper does not answer. Those
unanswered questions are not implementation details. They are the
difference between a search engine that tells the truth and one that
quietly conflates a login event with a logout event because the two
lines shared enough token structure to merge. This lecture is about
those six questions, the commitments Ourios makes in response, and the
honesty contract those commitments form with the user.

## Thesis

> **Drain is not a log parser. Drain is a tree. What makes it safe to
> put into production is everything we build around the tree — the
> confidence scoring, the merge auditing, the body retention, the
> reconstruction property — none of which appear in the paper.**

Hold on to that sentence. Every figure in this talk exists to defend
it.

## Learning objectives

By the end of this lecture you should be able to:

1. Draw the Drain parse tree from memory and walk a log line through
   it.
2. Name the six gaps between the published algorithm and a production
   log backend, and state the Ourios invariant that fills each gap.
3. Explain why bit-identical body reconstruction is a property test
   and not a unit test.
4. Defend the thesis above against a critic who says "just use
   zstd."

## Outline

| § | Topic                                         | Minutes |
|---|-----------------------------------------------|---------|
| 1 | Motivation: where the compression comes from  | 5       |
| 2 | The paper: Drain as published                 | 10      |
| 3 | Worked example: a line walks the tree         | 5       |
| 4 | What the paper does not say                   | 8       |
| 5 | The Ourios extensions                         | 8       |
| 6 | The honesty contract: reconstruction          | 5       |
| 7 | What is still open                            | 2       |
| — | Questions                                     | 2       |

---

## 1. Motivation: where the compression comes from

I want to start with a number, because the number is what makes this
whole project coherent. Operators of large log deployments — people
running Loki, Elasticsearch, proprietary SIEMs — consistently report
that their raw log volume compresses by somewhere between fifty and
two hundred times when it lands in a structured backend. That
compression does not come from zstd. If you zstd a day of raw logs
you get maybe ten times. The rest — the factor of five to twenty on
top of the byte codec — comes from noticing that your logs are not
really text at all.

They are a program output. The program has maybe two thousand
`printf`-style call sites. Each call site fires somewhere between a
few hundred and a few million times a day, always with the same
template and different parameters. A log line that reads

```
ERROR db connection failed for user 42 after 3 retries
```

is not a string. It is a tuple. It is template number, say, 847,
plus the parameters `(42, 3)`. The template itself appears once per
deployment. The parameters appear once per event. If you store the
template once and the parameters inline, you have already compressed
the log before you have compressed a single byte.

This is not a theoretical claim. It is how every serious log backend
built in the last decade actually works under the hood. What differs
between backends is how they recover the templates. You can ask
developers to annotate them at compile time — SLF4J's structured
logging, OpenTelemetry's log records — but the reality of a
heterogeneous deployment is that you inherit a pile of logs from
Python scripts and Go services and JVM apps and legacy C++ daemons,
and the only common substrate you have is the emitted text.

So you mine the templates online, from the text, as the logs flow.
That is what Drain does.

## 2. The paper: Drain as published

The Drain paper — He, Zhu, Zheng, and Lyu, *ICWS 2017* — introduces a
single data structure and one algorithm that walks it. The data
structure is a tree with a fixed depth. The algorithm is: preprocess
the line, walk the tree from root to leaf, decide at the leaf whether
this line matches an existing log group or opens a new one. That is
the whole paper. Ten pages.

Let me draw the tree.

### Figure 1 — The Drain parse tree

```
                         ┌─────────────┐
                         │    root     │
                         └──────┬──────┘
                                │
              ┌─────────────────┼─────────────────┐
              │                 │                 │
        ┌─────▼─────┐     ┌─────▼─────┐     ┌─────▼─────┐
        │  len = 5  │     │  len = 7  │     │  len = 11 │
        └─────┬─────┘     └─────┬─────┘     └─────┬─────┘
              │                 │                 │
      ┌───────┴──────┐          │                 │
      │              │          │                ...
  ┌───▼───┐      ┌───▼───┐  ┌───▼───┐
  │ tok₀= │      │ tok₀= │  │ tok₀= │
  │"INFO" │      │"ERROR"│  │"ERROR"│
  └───┬───┘      └───┬───┘  └───┬───┘
      │              │          │
    [ leaf         [ leaf     [ leaf
      log            log        log
      groups ]       groups ]   groups ]
```

Three levels matter here. The root has a child per distinct token
count — Drain assumes that two log lines of different length are
probably from different call sites, and this is empirically true
often enough to use as a cheap first-level filter. Below the length
node sits a chain of prefix nodes — one per token, up to a configured
depth. At depth two, as drawn, the tree branches on the first token
of the line. If the depth were three you would also branch on the
second token, and so on. The paper defaults to depth three or four;
the deeper you go, the more precise the partition but the more groups
you end up with.

At the bottom of each prefix chain is a leaf. A leaf is not a single
template. It is a *list* of templates — what the paper calls log
groups — each with its own parameter positions. When a line arrives
at a leaf, Drain compares it against each log group in the leaf by
token-wise similarity, picks the best match if the similarity exceeds
a threshold, and either merges the line into that group or, if no
group is similar enough, opens a new group.

The similarity function is where the arithmetic lives. It is simply
the fraction of positions where the template and the line have the
same token — wildcards count as matches. So if a leaf contains the
template `ERROR db connection failed for user <*>` and a line arrives
reading `ERROR db connection failed for user 42`, every token matches
— the wildcard absorbs the `42` — and similarity is 1.0. A different
line, `ERROR db connection timeout for user 7`, matches six of seven
tokens — `connection` matches, but `timeout` does not equal `failed`
— so similarity is about 0.86. If the threshold `st` is 0.7, both
lines land in the same group; the template widens to
`ERROR db connection <*> for user <*>`. If the threshold is 0.9,
only the first line matches; the second opens a new group.

That is Drain. That is the whole thing. I am not hiding complexity.
The paper is short because the algorithm is short.

## 3. Worked example: a line walks the tree

Let us walk one line through concretely so the abstraction has
weight.

### Figure 2 — Walking `ERROR db connection failed for user 42`

```
Line: "ERROR db connection failed for user 42"

Step 1 — preprocess
    tokens: ["ERROR", "db", "connection", "failed",
             "for", "user", "42"]
    length: 7

Step 2 — walk
    root          →  len=7 node
    len=7         →  tok₀="ERROR" branch
    tok₀="ERROR"  →  leaf L₇

Step 3 — compare at leaf L₇
    candidate A: "ERROR db connection failed for user <*>"
                 similarity = 7/7 = 1.00   ← best

    candidate B: "ERROR db pool exhausted for user <*>"
                 similarity = 5/7 = 0.71

Step 4 — decide
    threshold st = 0.7
    similarity(A) ≥ st   →  assign to group A
    param extracted: ["42"]
    template unchanged (already fully general at that slot)

Result
    template_id  = hash("ERROR db connection failed for user <*>")
    params       = ["42"]
```

Pause on step three. The whole engine is visible here. Every
decision Drain makes — whether to match, whether to widen, whether to
open a new group — is a function of that similarity score and that
one threshold. Lift the threshold and you get more, narrower
templates. Lower it and you get fewer, more abstract templates that
absorb lines they arguably should not absorb.

That single scalar is the most important knob in the whole system.
Remember the thesis: *what makes it safe to put into production is
everything we build around the tree.* We are about to talk about what
the paper does not say about the threshold, and about much else.

## 4. What the paper does not say

I want to go through this carefully, because these are the questions
that become bugs in production if you skip them.

### 4.1 It does not say what the threshold should be for your corpus

The paper reports empirical results on a handful of public corpora
with thresholds around 0.4 to 0.7. These are the datasets the authors
had access to — HDFS, BGL, Apache, OpenSSH. Your corpus is not one of
those. The right threshold for an application that emits heavily
templated, well-structured log lines is different from the right
threshold for an application that concatenates stack traces and
request payloads into each line.

This is not a criticism of the paper. This is a reminder that the
paper reports *that there exists a sweet spot*, not *what it is for
you*. In Ourios we default to a strict threshold — at least 0.7 —
and expose it as tenant-configurable, and we gate any reduction
below 0.7 behind an RFC. That last part matters. There is always an
engineer who, when templates look noisy, wants to lower the
threshold to "clean things up." What they are actually doing is
forcing unrelated templates to merge. A strict default plus a gate
keeps that pressure from silently drifting the system toward wrong.

### 4.2 It does not say what to do when similarity is close but not above threshold

Drain is a classifier with two classes: match, and no-match. In
practice there is a third case that matters deeply to a log backend.
Imagine a line that matches the best candidate at 0.65 when the
threshold is 0.7. What do you do? The paper says: open a new group.
The paper is right that this is the safe default, but it is wrong
that this is a complete answer. In a log backend the user has a
specific question: *was this line produced by the same code as that
template?* If you opened a new group because similarity was 0.65,
you have told the user "these are different" — but you only know
that with 0.65 confidence, not 1.0 confidence. A query that asks
"show me all events from template X" will miss this line even though
it came from the same call site, probably.

Ourios handles this with a three-zone model.

### Figure 3 — The three-zone confidence model

```
confidence →
0 ───────── floor ───────── threshold ──────────── 1
                │                │
                ▼                ▼
┌──────────────┐┌───────────────┐┌──────────────────┐
│ parse_failed ││ lossy match   ││ clean match      │
│              ││               ││                  │
│ retain body, ││ retain body + ││ template + params│
│ count error  ││ template, set ││ only; body opt.  │
│              ││ lossy_flag    ││                  │
└──────────────┘└───────────────┘└──────────────────┘
```

Three zones, three behaviours. Above the threshold, the happy path:
store the template id and the parameters. Below the threshold but
above the floor — what I am calling the lossy zone — store the
template id, the parameters, *and* the original body, and raise a
flag on the row so the reader knows not to trust reconstruction
against this row. Below the floor, parse failed altogether: store
only the body, increment `parse_failures_total`, and move on.

The floor is the second most important knob in the system. Set it
too low and you never see parse failures — everything is technically
a match, just a bad one. Set it too high and you throw away useful
partial matches. A reasonable default sits around 0.3. The point is
that the three-zone model exists at all, because without it the
backend is lying to the user in the lossy zone.

### 4.3 It does not say what to do when parameters are enormous

The paper implicitly assumes parameters are short variable bits —
numbers, hostnames, UUIDs. In production a parameter slot may
capture an entire stack trace, a request body, a base64 payload. If
you put a megabyte of stack trace into a parameter, Parquet's
dictionary encoding collapses. File sizes explode. Query latency
degrades. The backend's whole value proposition evaporates for
that column.

The Ourios answer is a per-parameter byte limit — 256 bytes by
default — with overflow behaviour that is explicit rather than
clever. When a parameter exceeds the limit, the original value
spills into the `body` column of the row, the `params` slot gets a
short truncation marker, and a counter increments. Per-service
alerts fire when more than 1% of rows hit overflow. The ceiling on
the limit is 1 KiB; above that we would rather open an RFC than
silently accept larger values.

This is the kind of rule that looks ugly on a whiteboard and is
invisible in a paper but saves the storage format from a class of
tail-latency failure that is otherwise impossible to diagnose in
production.

### 4.4 It does not say whether to preserve whitespace

The paper talks about tokens. Tokens are a convenient abstraction
and they are also a *lossy* abstraction. When you tokenise
`connection   failed` — two words separated by three spaces — into
`["connection", "failed"]`, you have thrown away the three spaces.
Later, when an operator opens the UI and asks "show me what was
actually logged," and you reconstruct from template plus parameters,
you produce `connection failed` — one space. You have lied. Quietly,
in a way that the user will only notice if they happen to be
debugging a whitespace-sensitive format.

This is the invariant in CLAUDE.md §3.3 — **bit-identical body
reconstruction** — and it is stricter than it sounds. It says: for
every line we ingest, either we can reproduce the original byte
stream exactly from what we stored, or we have flagged the row as
lossy. No in-between. The miner either captures the inter-token
whitespace as part of the template, or it gives up honestly and
keeps the body.

### 4.5 It does not say how templates evolve over time

A service ships a new version. The log format changes — a new field
appears, an old one goes away, word order shifts. The template tree
you built from last month's logs no longer matches this month's logs
cleanly. The paper has nothing to say about this; it assumes a
static tree.

Real deployments are never static. Ourios needs a template
versioning story: what changes cause a new template version vs. a
new template, what aliases hold between old and new templates, and
how a query that says "template X" either resolves across versions
or surfaces the drift explicitly to the user. This is hazard 5 in
`docs/hazards.md` and it is genuinely hard — hard enough that the
RFC has it as an open question rather than a solved problem.

### 4.6 It does not say anything about multi-tenancy

The paper describes one tree. A log backend serves many tenants
whose logs cannot cross-pollinate: tenant A's `login` template must
not end up merged with tenant B's `logout` template just because
they share token structure. This is CLAUDE.md §3.7, and it is the
invariant that says the tree is not one tree — it is one tree *per
tenant* — and every code path that touches data carries a tenant
id. Retrofitting this after the fact is more expensive than building
it in at the start; the RFC makes it foundational.

### Figure 4 — Gaps to invariants

| What the paper doesn't say              | Ourios invariant (CLAUDE.md) |
|-----------------------------------------|------------------------------|
| What threshold to pick                  | §3.1 — strict default ≥ 0.7, RFC gate below |
| What to do in the lossy zone            | §3.1 — three-zone model, body retained under threshold |
| What to do with huge parameters         | §3.2 — 256 B limit, overflow to body, 1% alert |
| Whether whitespace is preserved         | §3.3 — bit-identical reconstruction or lossy flag |
| How templates evolve                    | §3.5 — versioning, aliases, drift detection |
| How tenants are isolated                | §3.7 — one tree per tenant, tenant id on every path |

This is the table to internalise. Everything else in the design
descends from these six lines.

## 5. The Ourios extensions: the record shape and the merge policy

Let me show you what a mined record looks like in Ourios, because it
makes the invariants concrete.

### Figure 5 — The Ourios log record

```
┌──────────────┬──────────────────┬─────────────────┐
│ tenant_id    │ template_id      │ template_ver    │
├──────────────┼──────────────────┼─────────────────┤
│ params[]     │ body?            │ confidence      │
├──────────────┼──────────────────┼─────────────────┤
│ lossy_flag   │ timestamp        │ service         │
└──────────────┴──────────────────┴─────────────────┘
```

Every field on that diagram is a commitment:

- `tenant_id` is present on every row, not on every file — the
  partitioning is a separate question. We never trust the file to
  tell us the tenant; we trust the row.
- `template_id` is the identity of a template within a tenant. The
  same text in two tenants yields two different ids. This is
  deliberate — it means a query never needs to join across tenants
  to resolve identity.
- `template_version` lets a template's representation change over
  time while the logical identity persists.
- `params` are length-bounded per 4.3 above.
- `body?` is present whenever the lossy-or-fail zone fired, and
  optionally always, as a tenant-configurable choice. Paying the
  storage cost of always keeping the body buys perfect
  reconstructability; most tenants will not want to pay it, and the
  default should be "only when needed."
- `confidence` is the scalar the three-zone model was defending.
- `lossy_flag` is the boolean the reader checks before trusting
  template-based rendering.

Now the other piece the paper does not address — merging.

Drain as published merges templates implicitly. When a line matches
an existing log group but its tokens differ at some positions, the
template at those positions becomes a wildcard. The template has
widened. This is a merge. The paper does not call it that and does
not audit it.

In Ourios, every widening event that crosses a configurable
threshold of semantic change fires a merge audit event — a
structured record with the old template, the new template, the
tenant, the timestamp, and the reason. The audit event is a
first-class citizen: it goes to the same storage, it is queryable,
and there is a metric `merges_total` that dashboards the rate.

Why does this matter? Because the horror story for a template miner
is a silent merge that crosses a semantic boundary. `user logged
in <*>` and `user logged out <*>` differ in one token. Depending on
your threshold, they can merge into `user logged <*> <*>`, and now a
query for the login event returns logout events too. The user will
not know this has happened unless we tell them. The audit event is
how we tell them.

Strict defaults plus visible audits plus a merge-rate metric are not
paranoia. They are the shape of "we are not going to let this system
lie to you silently."

## 6. The honesty contract: reconstruction as a property

We have seen confidence scoring, length limits, whitespace capture,
versioning, tenancy, merge auditing. There is one more piece that
ties them together, and it is less a design and more a claim we make
to the user.

### Figure 6 — The reconstruction invariant

\\[
\begin{aligned}
&\forall\\, \mathtt{line} \in \mathtt{corpus}: \\\\
&\quad \mathtt{reconstruct}(\mathtt{mine}(\mathtt{line})) \equiv \mathtt{line} \\\\
&\quad \lor\\;\\; \mathtt{mine}(\mathtt{line}).\mathtt{lossy\\_flag} = \mathtt{true}
\end{aligned}
\\]

In English: for every log line we ingest, either we can reproduce
the exact bytes the customer's application wrote, or we flag the row
so the reader knows not to claim we can.

This is not a design decision. It is a property. It is what we prove
on every CI run. The test is:

```
for every line in testdata/corpus/ :
    record = mine(line)
    if record.lossy_flag == false :
        assert reconstruct(record) == line
```

If that assertion ever fails, the backend is lying, and that PR does
not merge.

The reason this is a property test and not a unit test is that the
set of log lines we care about is the power set of our token
vocabulary, and we cannot write unit tests against a power set. What
we can do is assemble a corpus — real, anonymised log lines from
real applications — and run the property against every line in the
corpus on every build. `proptest` lets us go further: it generates
synthetic adversarial inputs that stress the whitespace capture, the
tokeniser, the length limits, and the merge policy, looking for a
counterexample. When it finds one, we have learned something real.

The reconstruction property is the single honesty contract between
this system and its operators. Everything else in the design — the
confidence model, the body retention, the merge audit — is in
service of making this property defensible.

## 7. What is still open

I am going to close with the things I do not yet know, because if
this lecture ended with a polished answer it would be a marketing
pitch and not a lecture.

- **Threshold on real corpora.** We have said "strict default, at
  least 0.7." The paper's sweet spot is corpus-dependent. Until we
  run Ourios on meaningful corpora we do not know whether 0.7 is
  merely safe or also *good*.
- **Masking placement.** Drain3 does regex-based masking — IPs,
  UUIDs, numbers — before the tree walk. This improves template
  stability dramatically but it also couples the tree to a set of
  regex rules that are inherently wrong at some edges. Where
  exactly that masking happens — pre-tree, post-tree, both, neither
  — is an open design question.
- **Binary and malformed input.** Log lines are not always valid
  UTF-8. They are not always text. A mature miner has a story for
  what happens when the input is simply not parseable into tokens.
  We do not yet have that story written down.
- **Template identity across versions.** The versioning story in
  §4.5 needs an alias mechanism and a drift query surface. Neither
  is designed yet.

These four items are in `docs/rfcs/0001-template-miner.md` under
Open Questions, and the RFC cannot move to accepted until they are
resolved.

## Thesis, restated

> **Drain is not a log parser. Drain is a tree. What makes it safe
> to put into production is everything we build around the tree —
> the confidence scoring, the merge auditing, the body retention,
> the reconstruction property — none of which appear in the paper.**

If you take one thing away from this lecture, take that sentence.
The tree is a reasonable default partition function over log lines.
The system around it is the product.

## Questions

*Prompts for the Q&A segment. Seed these into the room if the
audience is quiet.*

1. Why not use an LLM-based parser instead of Drain?
2. Why is reconstruction a property test and not a unit test — can
   you give an example of a bug that a unit test would miss?
3. How does the merge audit scale when a single deployment produces
   a high merge rate — does the audit stream itself need to be
   templated?
4. If a tenant configures a threshold below 0.7, how is that audited
   as a policy event?
5. What happens to the template tree when a service is sunset and
   its templates go cold?

## References

- He, P., Zhu, J., Zheng, Z., Lyu, M.R. *Drain: An Online Log Parsing
  Approach with Fixed Depth Tree.* ICWS 2017.
- Drain3 (IBM): https://github.com/logpai/Drain3
- LogPAI benchmark suite: https://github.com/logpai/logparser
- Ourios: `CLAUDE.md` §2.2, §3.1–§3.3, §3.5, §3.7, §4, §6.2, §6.3
- Companion RFC: `docs/rfcs/0001-template-miner.md`
