---
rfc: 0057
title: "meta: move CLAUDE.md to AGENTS.md so every AI coding agent gets the same project context"
status: drafted
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-09-27
supersedes: —
superseded-by: —
---

# RFC 0057 — meta: move CLAUDE.md to AGENTS.md

> **This is a `meta:` RFC.** It proposes a change to `CLAUDE.md`, whose
> footer declares it load-bearing: *"further changes require a `meta:`
> RFC and majority maintainer approval."* Per `CLAUDE.md` §8.5 the edit
> is **not** made in the drafting session: this RFC specifies the exact
> change, and the move lands as a separate PR after approval. Precedent:
> RFC 0012 (the §2 pillar-#2 rewording), drafted and enacted the same
> way.

## 1. Summary

The project context every contributor's agent must read lives in
`CLAUDE.md`, a file name only Claude Code loads. This RFC moves it,
with `git mv`, to `AGENTS.md`, the open convention that Codex, Cursor,
GitHub Copilot's coding agent, Jules and others read. Every section
number stays the same. `CLAUDE.md` becomes a one-line stub that imports
`AGENTS.md`, so Claude Code sessions load the same text as today and the
575 existing `CLAUDE.md` citations (507 of them `CLAUDE.md §N`) keep
resolving without being rewritten. The handful of Claude-specific lines
are reworded to be agent-neutral; no rule changes meaning.

## 2. Motivation

### 2.1 The context is Claude-only by file name, not by content

Ourios is public OSS and states that AI-assisted contributions follow
the same review, CI and RFC process as human ones (`CLAUDE.md` §9). The
invariants in §3 (no silent merges, WAL-before-ack, tenancy) are exactly
what an agent must know before touching the hot path. Today a
contributor using Codex, Cursor, Copilot or Gemini CLI gets none of it
unless they notice the file and paste it in by hand. The content is
already almost entirely agent-neutral (§3.4 below counts the exceptions),
so the barrier is the file name.

### 2.2 AGENTS.md is the shared convention

`AGENTS.md` is a plain-markdown file at the repository root that
multiple agents read natively (§3.6 lists which, with citations). One
canonical file, read by all of them, keeps a single source of truth
instead of `CLAUDE.md` + `.github/copilot-instructions.md` +
`GEMINI.md` + `.cursor/rules/*` drifting apart.

### 2.3 Why move rather than copy

A copy creates two load-bearing documents with one approval rule, and
they drift. A move keeps one text, keeps `git log --follow` history on
it, and keeps blame useful for the footer's changelog.

## 3. Proposed design

### 3.1 The move

In the enacting PR, and as its **first commit** so rename detection is
unambiguous:

```sh
git mv CLAUDE.md AGENTS.md
```

That commit changes no content. The rewording in §3.4 lands in a second
commit of the same PR, so `git log --follow AGENTS.md` walks through
the rename into the full `CLAUDE.md` history, and the diff reviewers
approve is the wording diff alone.

**Section numbering is frozen.** Every heading `## 1.` … `## 10.` and
every `### N.M` keeps its number and its position. Headings may be
reworded (§3.4) but not renumbered, merged, split or reordered. New
material, if any, is appended as a new last subsection of an existing
section (e.g. a future §8.6), never inserted.

### 3.2 The CLAUDE.md stub

After the move, a new `CLAUDE.md` is created containing exactly:

```markdown
@AGENTS.md
```

Claude Code's memory documentation specifies that *"CLAUDE.md files
can import additional files using `@path/to/import` syntax"*, that
*"relative paths resolve relative to the file containing the import"*,
with *"a maximum depth of four hops"*, and that import parsing skips
code spans and fenced code blocks (so the backticked `@path` mentions
elsewhere in the file are inert) [CC-mem]. The same page documents the
bridge this RFC uses: *"A `CLAUDE.md` containing `@AGENTS.md`: you can
leave it. Keeping the import never makes Claude read `AGENTS.md`
twice."*

The stub is **required, not a courtesy.** Claude Code reads
`AGENTS.md` natively, but by default *"only when you have no
`CLAUDE.md` in your working directory or above it"* [CC-mem]. Without
the stub, a contributor with an older Claude Code, or one whose
`~/.claude/CLAUDE.md` or a parent directory's `CLAUDE.md` exists, would
load nothing from this repo. With it, every Claude Code version that
supports imports loads the full text.

The approval dialog Claude Code shows on first use applies only to
*external* imports, those that *"resolve outside your working
directory"* [CC-mem]; `AGENTS.md` sits beside the stub, so no dialog
appears. RFC0057.3 checks the loaded context rather than assuming it.

The stub is one line on purpose: Claude-only prose below it would
reintroduce the split this RFC removes. Changes to the stub fall under
the same `meta:` rule as `AGENTS.md` (§3.4.9).

### 3.3 Citations are not rewritten

Measured on `main` at `d8c8208` (2026-09-27):

| Measure | Command | Count |
|---|---|---|
| Occurrences of `CLAUDE.md` | `git grep -o 'CLAUDE\.md' \| wc -l` | **575** |
| Lines containing it | `git grep -c 'CLAUDE\.md'` summed | **566** |
| Files containing it | `git grep -l 'CLAUDE\.md' \| wc -l` | **159** |
| Section citations `CLAUDE.md §N[.M]` | `git grep -oE 'CLAUDE\.md.? ?§ ?[0-9]+(\.[0-9]+)*' \| wc -l` | **507** |

By area (occurrences / files; a markdown link
`` [`CLAUDE.md`](CLAUDE.md) `` counts twice):

| Area | Occurrences | Files |
|---|---:|---:|
| `docs/rfcs/NNNN-*.md` (historical decision records) | 307 | 52 |
| `crates/` (code comments, test docs) | 147 | 79 |
| `docs/` other than RFC files (incl. `docs/rfcs/README.md`) | 78 | 9 |
| `.claude/skills/rfc-check/SKILL.md` | 9 | 1 |
| Everything else (README, CONTRIBUTING, CI, Helm, `justfile`, …) | 34 | 18 |

Section numbers cited, with counts: §1 (32), §2 (41), §2.1 (1), §2.2
(1), §3 (19), §3.1 (25), §3.2 (17), §3.3 (37), §3.4 (70), §3.5 (18),
§3.6 (29), §3.7 (55), §4 (26), §4.6 (13), §5 (1), §5.1 (5), §5.4 (1),
§6 (1), §6.1 (6), §6.2 (64), §6.3 (20), §6.4 (1), §6.6 (3), §6.7 (1),
§7 (15), §8.3 (1), §8.5 (1), §9 (1), §10 (2). Every one exists in the current
`CLAUDE.md`: §2.1, §2.2 and §4.6 cite numbered list items (pillar #1,
pillar #2, hazard #6), not headings, and those lists keep their order
under §3.1's freeze.

**Rule.** Because the stub remains and section numbers are frozen, a
citation `CLAUDE.md §N` resolves to the same text after the move as
before it (open `CLAUDE.md`, follow the import, find §N). So:

- **RFCs are not touched.** They are historical records; an accepted
  RFC's text changes only through the routes `docs/rfcs/README.md`
  §Lifecycle allows, and a file rename is not one of them.
- **Code comments, CI, Helm and tooling comments are not touched.**
  They resolve via the stub; they may be updated opportunistically when
  the surrounding line is edited for another reason.
- **New writing cites `AGENTS.md §N`** from the enacting PR onward,
  including new RFCs. The RFC template's "`CLAUDE.md` sections
  constrained" wording in `docs/rfcs/README.md` §Required sections is
  updated in the follow-up (§3.7).

### 3.4 Agent-neutral wording (exact replacements)

`grep -niE 'claude|anthropic|\.claude|sub-agent|Explore|model|prefix'
CLAUDE.md` finds every Claude-specific line. Each is listed with its
replacement. Nothing else in the file changes except the footer (§3.4.9).

**3.4.1 Title (line 1).**

> `# CLAUDE.md — Ourios`

becomes

> `# AGENTS.md — Ourios`

**3.4.2 §5.2 (line 194).** *"This is for humans and for Claude
equally"* becomes *"This is for humans and for AI agents equally"*.

**3.4.3 §7 layout.** The tree gains the two root files and the skills
comment is neutralised:

```
├── AGENTS.md                 # project context for humans and AI agents
├── CLAUDE.md                 # one-line stub importing AGENTS.md
…
└── .claude/
    └── skills/               # project skills (plain markdown, any agent)
```

The `.claude/` directory does not move (§3.5).

**3.4.4 §8 heading.** `## 8. Context management (agent-specific)` is
kept verbatim: "agent-specific" already means "for agents", and the
section number is frozen. A one-sentence lead is added under it:

> These rules apply to any AI coding agent. Tool names in parentheses
> are examples, not requirements.

**3.4.5 §8.1–§8.4.** General; they describe context decay, sub-agents,
scratch files and grep, which every current agent has. Only §8.2 names a
Claude Code feature. Its last sentence becomes:

> Use a read-only research agent (e.g. Claude Code's `Explore`) for
> research, and a fresh agent in an isolated worktree for write tasks.

**3.4.6 §8.5 "Cache discipline".** The rules are general (prompt
prefix caching and context forking exist across vendors) but the first
line names "this file". Replacement body:

> The system prompt, tools, and this file (loaded directly or through a
> tool's import, e.g. `CLAUDE.md`) are cached as a prefix by most agents.
> - Do not suggest mid-session model switches.
> - Do not edit this file in the same session where its rules will be
>   applied downstream — that invalidates the prefix for the rest of the
>   session.
> - For long sessions, compact into `context-log.md` and fork rather than
>   grow.

The Claude-Code-specific parts of §8 (the `Explore` name, the `@`
import) therefore stay as parenthetical examples in `AGENTS.md` rather
than moving to `CLAUDE.md`, keeping the stub a single line (§3.2).

**3.4.7 §9 heading and body.** `## 9. Collaboration with non-Claude
contributors` becomes `## 9. AI-assisted contributions`. Body:

> Ourios is public OSS. AI-assisted code, whichever assistant produced
> it, goes through the same review, CI, and RFC process as any human
> contribution. Do not self-merge. Do not label a PR as ready for review
> until CI is green and the PR description addresses any invariant
> listed in §3 or hazard in §4 that the change touches.
>
> Attribution: AI-assisted commits carry a `Co-Authored-By:` trailer
> naming the assistant that helped (e.g. `Co-Authored-By: Claude
> <noreply@anthropic.com>`, or the equivalent trailer the tool emits).
> RFCs authored with an AI assistant list the human driver as the
> author and name the assistant in the `drafting-assistance:` header
> field (e.g. `drafting-assistance: Claude`, `drafting-assistance:
> Codex`).

**3.4.8 Intro paragraph.** *"This document is your project context"*
is already neutral and stays.

**3.4.9 Footer.** Add a dated changelog sentence in the existing style
(*"2026-MM-DD revision moves the file to `AGENTS.md` (`CLAUDE.md` is now
a stub importing it) and makes §§5.2, 7, 8.2, 8.5 and 9 agent-neutral,
per **RFC 0057** (maintainer-approved `meta:` RFC)."*) and bump *Last
updated*. The closing rule becomes *"This document is load-bearing;
further changes to it (or to the `CLAUDE.md` stub) require a `meta:`
RFC and majority maintainer approval."*

### 3.5 Skills: `.claude/skills/` stays where it is

`.claude/skills/rfc-check/SKILL.md` and `.claude/skills/openfga/` are
plain markdown with YAML front matter; any agent can read them, and
`AGENTS.md` §7 now says so. The directory **does not move**, because:

1. Claude Code discovers project skills only under `.claude/skills/`;
   moving them would break the one agent that loads them automatically.
2. `skills-lock.json` pins `openfga` from `openfga/agent-skills`, and
   the installer writes to `.claude/skills/`; relocating means fighting
   the tool on every update.
3. There is no competing cross-agent skills location to move to
   (§3.6: none of the other tools documents one it reads from this
   repo). If one emerges, a symlink or a follow-up RFC handles it.

`AGENTS.md` points to it through §7's tree comment (§3.4.3); no other
section gains text.

**Note: a nested `AGENTS.md` already exists** at
`.claude/skills/openfga/AGENTS.md` (vendored upstream content). Tools
treat nested files differently: Copilot applies the nearest one, so
inside that directory it would see the OpenFGA guide *instead of* the
root rules [GH-instr]; Codex concatenates root and nested files, so it
sees both, subject to its byte budget [Codex-src]. Work inside a
vendored skill directory is rare and never touches the hot path, so
this is accepted and the file is not edited.

### 3.6 Per-tool support

Checked 2026-09-27. Where the vendor's documentation site was
unreachable from the drafting environment, the vendor's own
documentation or source on GitHub was read instead; rows marked
**unverified** rest on secondary sources and must be re-checked by the
enacting PR's author.

| Tool | Reads root `AGENTS.md` | Reads `CLAUDE.md` | Own file needed? | Caveats |
|---|---|---|---|---|
| Claude Code | Yes, but only when no `CLAUDE.md` exists in or above the working directory [CC-mem] | Yes (primary) | No: the stub (§3.2) is the bridge | Import depth 4 hops; skips imports in code spans |
| OpenAI Codex CLI | Yes, natively; `AGENTS.override.md` takes precedence; files concatenated from the git root down to the working directory [Codex-src] | No | No | `project_doc_max_bytes` default 32 768; excess is truncated [Codex-cfg] |
| GitHub Copilot (coding agent, code review) | Yes, anywhere in the tree, nearest file wins [GH-instr] | Yes, "alternatively", a single root `CLAUDE.md` or `GEMINI.md` [GH-instr] | No; `.github/copilot-instructions.md` is optional | Code review lists `AGENTS.md` as a shared-rules source [GH-review] |
| Gemini CLI | **Only if configured** via `context.fileName` [Gemini-md] | No | **Yes, a setting** (below) | Default context file is `GEMINI.md` |
| Cursor | Yes, root and nested (**unverified**, secondary sources) | Unconfirmed | No | `cursor.com/docs` unreachable when drafted |
| Jules | Yes, root (**unverified**, secondary sources) | Not stated | No | `jules.google` unreachable when drafted |

**Consequences for Ourios.**

- **No `.github/copilot-instructions.md`, no `.cursor/rules/`, no
  `GEMINI.md`.** Copilot, Codex, Cursor and Jules read `AGENTS.md`
  directly.
- **Gemini CLI is the one genuine gap.** It reads `AGENTS.md` only when
  `context.fileName` lists it. The documented example is
  `{"context":{"fileName":["AGENTS.md","CONTEXT.md","GEMINI.md"]}}`
  [Gemini-md]. The enacting PR adds a project-level
  `.gemini/settings.json` containing only
  `{"context":{"fileName":["AGENTS.md"]}}`: a pointer, not a copy of
  any rule, so it cannot drift. If the maintainer prefers no Gemini
  file in the repo, the alternative is one sentence in `CONTRIBUTING.md`
  telling Gemini CLI users to set it in their user settings (§7).
- **Codex's 32 KiB budget.** `AGENTS.md` is 19 021 bytes today, well
  inside it. The vendored `.claude/skills/openfga/AGENTS.md` is
  104 117 bytes; Codex concatenates it only when working inside that
  directory, and it is truncated there. That is upstream content and a
  pre-existing condition; noted, not changed.
- **Tools that read `CLAUDE.md` but not `AGENTS.md`, and do not expand
  `@` imports**, would see the single line `@AGENTS.md` instead of the
  rules they see today. Of the tools checked, Copilot reads `CLAUDE.md`
  only as an alternative to `AGENTS.md`, so it is not affected. For any
  other such tool the stub is still a readable pointer to the right
  file, but it is a regression for that tool; RFC0057.3 covers Claude
  Code only, and a contributor report is the signal to revisit.
- **Line-count warning.** Claude Code warns on memory files over 200
  lines [CC-mem]; `CLAUDE.md` is 424 lines today, so this is unchanged
  by the move.

### 3.7 Follow-up: living documents that switch to `AGENTS.md`

A small PR after the enacting one (not blocked on it being the same
PR; no `meta:` RFC needed because it changes no rule) rewrites the
citations in **living** documents only:

| File | Occurrences |
|---|---:|
| `README.md` | 4 |
| `CONTRIBUTING.md` | 1 |
| `CODE_OF_CONDUCT.md` | 2 |
| `docs/introduction.md` | 1 |
| `docs/hazards.md` | 13 |
| `docs/verification.md` | 16 |
| `docs/benchmarks.md` | 19 |
| `docs/glossary.md` | 9 |
| `docs/roadmap.md` | 6 |
| `docs/rfcs/README.md` (process doc, not an RFC) | 9 |
| `.claude/skills/rfc-check/SKILL.md` | 9 |
| `.github/pull_request_template.md` | 0 (see below) |

`.github/pull_request_template.md` cites neither file today; the
follow-up adds one checklist line, *"PR description addresses any
`AGENTS.md` §3 invariant or §4 hazard this change touches"*, since that
is the rule §9 states.

Explicitly **not** in the follow-up: `docs/rfcs/NNNN-*.md` (historical),
`docs/talks/` (dated lectures), `CHANGELOG.md` (history), `crates/`,
`.github/workflows/`, `.github/scripts/`, `deploy/`, `justfile`,
`codecov.yml`, `rust-toolchain.toml`, `.gitignore`, `.bestpractices.json`
and `docs/SUMMARY.md`'s RFC 0012 entry (its title names the file it
changed at the time). All of these resolve through the stub.

## 4. Alternatives considered

**Keep `CLAUDE.md`, add `AGENTS.md` as a copy.** Two load-bearing files
under one approval rule; a `meta:` change must land in both or they
drift, and nothing enforces it. Rejected for the reason §2.3 gives.

**Keep `CLAUDE.md` canonical, make `AGENTS.md` the stub.** Other agents
do not implement `@path` imports (§3.6), so they would read a single
line and nothing else. Only the reverse direction works.

**Symlink `CLAUDE.md` → `AGENTS.md`.** Works for tools that follow
symlinks, but symlinks are fragile on Windows checkouts
(`core.symlinks=false` materialises a text file containing the path),
render as a one-line file on GitHub, and some tools do not follow them.
The documented import is explicit and portable. Rejected.

**Rewrite all 575 citations to `AGENTS.md`.** A 159-file diff, 52 of
them RFCs whose text is historical and, when accepted, terminal. It
buys nothing the stub does not already give. Rejected; living docs only
(§3.7).

**Per-tool files (`.github/copilot-instructions.md`, `GEMINI.md`,
`.cursor/rules/`).** Each is another copy to keep in sync. §3.6 finds
only Gemini CLI unable to read `AGENTS.md` by default, and that gap is
closed by a one-key setting that points at `AGENTS.md`, not by a copy.

**Move `.claude/skills/` to a neutral path.** Breaks Claude Code's
discovery and the skills installer for no reader gain (§3.5).

## 5. Acceptance criteria

These are doc-state assertions checked on the enacting PR, like
RFC 0012's. Each carries a runnable check.

> **Scenario RFC0057.1 — history follows the move.**
> - **Given** the enacting PR's first commit
> - **When** `git show --stat --find-renames=100% <commit>` is run
> - **Then** it reports exactly one change, `CLAUDE.md => AGENTS.md`
>   with 100% similarity
> - **And** `git log --follow --oneline AGENTS.md` lists the commits
>   that previously touched `CLAUDE.md` (e.g. `b50067d`)

> **Scenario RFC0057.2 — section numbers are unchanged.**
> - **Given** `CLAUDE.md` on `main` immediately before the enacting PR
>   and `AGENTS.md` after it
> - **When** both are reduced to their heading numbers with
>   `grep -oE '^#{2,3} [0-9]+(\.[0-9]+)?' FILE`
> - **Then** the two lists are identical, in the same order
> - **And** the numbered pillar list in §2 and hazard list in §4 have
>   the same item count as before (so §2.1, §2.2 and §4.6 still resolve)

> **Scenario RFC0057.3 — the stub imports AGENTS.md and Claude Code
> loads it.**
> - **Given** the enacted repository
> - **When** `cat CLAUDE.md` is run
> - **Then** its entire content is the single line `@AGENTS.md`
> - **And** in a fresh Claude Code session at the repo root, `/memory`
>   (or asking the agent to quote §3.4's first rule) shows `AGENTS.md`'s
>   content loaded

> **Scenario RFC0057.4 — no broken `CLAUDE.md §N` citation.**
> - **Given** the enacted repository
> - **When** every section number cited as `CLAUDE.md §N[.M]` or
>   `AGENTS.md §N[.M]` is extracted with
>   `git grep -hoE '(CLAUDE|AGENTS)\.md.? ?§ ?[0-9]+(\.[0-9]+)*'`
> - **Then** each number resolves to a heading in `AGENTS.md`, or to
>   item *M* of the numbered list in section *N* (§2, §4)
> - **And** the set of cited numbers is a subset of those that resolved
>   before the move (no citation newly points at nothing)

> **Scenario RFC0057.5 — the only wording changes are §3.4's.**
> - **Given** the enacting PR's second commit
> - **When** its diff against the first is reviewed
> - **Then** every hunk corresponds to an item in §3.4.1–§3.4.9
> - **And** `grep -niE 'claude|anthropic' AGENTS.md` matches only the
>   footer's history, §7's `.claude/` and `CLAUDE.md` stub lines, §8's
>   parenthetical examples, and §9's example trailer / header

> **Scenario RFC0057.6 — no per-tool copies.**
> - **Given** the enacted repository
> - **When** `git ls-files` is searched for
>   `.github/copilot-instructions.md`, `GEMINI.md`, `.cursorrules` and
>   `.cursor/rules/`
> - **Then** none exists
> - **And** `.gemini/settings.json`, if added (§3.6), parses as JSON
>   whose `context.fileName` names `AGENTS.md` and holds no rule text

> **Scenario RFC0057.7 — the book still builds.**
> - **Given** the enacted repository
> - **When** `mdbook build` runs
> - **Then** it succeeds with no new warnings

## 6. Testing strategy

No code changes, so no `proptest`, corpus or `criterion` work
(`CLAUDE.md` §6.2 techniques do not apply). RFC0057.1, .2, .4 and .6 are
shell one-liners the enacting PR description runs and pastes;
RFC0057.3's second clause and RFC0057.5 are reviewer checks. As with
RFC 0012, the load-bearing gate is the footer's majority maintainer
approval on the enacting PR.

A CI check for RFC0057.4 (fail when a cited section number stops
existing) would protect the frozen numbering permanently; it is listed
as an open question rather than required, since it is new CI surface.

## 7. Open questions

- [ ] **Maintainer approval (majority)**, per `CLAUDE.md`'s footer.
- [ ] **CI guard for citations.** Add a `just`/CI step running
      RFC0057.4's check so a future renumbering fails the build? It
      would make §3.1's freeze enforceable rather than a convention.
- [ ] **Trailer wording.** §3.4.7 lets each tool emit its own
      `Co-Authored-By:` trailer. Should the project instead require one
      fixed form (e.g. the tool's product name and a no-reply address),
      so the history is greppable across tools?
- [ ] **`drafting-assistance:` values.** Free text naming the tool, or
      a short fixed list? `docs/rfcs/README.md`'s front-matter example
      (`drafting-assistance: Claude   # omit if no LLM drafted`) is
      updated in the follow-up either way.
- [ ] **Gemini CLI setting.** Commit `.gemini/settings.json`
      (recommended, §3.6) or document the user-level setting in
      `CONTRIBUTING.md` instead?
- [ ] **Unverified rows.** Cursor and Jules support rests on secondary
      sources (§3.6); the enacting PR's author re-checks
      `cursor.com/docs` and `jules.google/docs`.
- [ ] **Stub-only rule.** Is "the stub stays one line; Claude-specific
      text needs a `meta:` RFC" (§3.2) the right bar, or should small
      Claude Code notes be allowed below the import without one?

## 8. References

- **`CLAUDE.md`** footer (the `meta:` RFC + majority-approval rule),
  §7 (layout), §8 (agent context management), §8.5 (why the edit is not
  made in this session), §9 (attribution).
- **RFC 0012** — precedent `meta:` RFC amending `CLAUDE.md`.
- **`docs/rfcs/README.md`** — front matter (`drafting-assistance:`),
  §Lifecycle (why accepted RFCs are not rewritten).
- **`.claude/skills/rfc-check/SKILL.md`** — classifies `CLAUDE.md`
  changes as RFC-required; updated in the §3.7 follow-up.
- **[CC-mem]** Claude Code, *Manage Claude's memory*:
  <https://code.claude.com/docs/en/memory> (imports, `AGENTS.md`
  handling, external-import approval).
- **[Codex-src]** `openai/codex`, `codex-rs/core/src/agents_md.rs`:
  <https://github.com/openai/codex/blob/main/codex-rs/core/src/agents_md.rs>.
- **[Codex-cfg]** `openai/codex`, `codex-rs/config/defaults.toml`
  (`project_doc_max_bytes`, `project_doc_fallback_filenames`):
  <https://github.com/openai/codex/blob/main/codex-rs/config/defaults.toml>.
- **[GH-instr]** GitHub Docs, *Adding repository custom instructions
  for GitHub Copilot* (source: `github/docs`,
  `content/copilot/how-tos/copilot-on-github/customize-copilot/add-custom-instructions/add-repository-instructions.md`).
- **[GH-review]** GitHub Docs, *About Copilot code review* (source:
  `github/docs`, `content/copilot/concepts/agents/code-review.md`).
- **[Gemini-md]** Gemini CLI, *GEMINI.md files*:
  <https://github.com/google-gemini/gemini-cli/blob/main/docs/cli/gemini-md.md>.
- **AGENTS.md convention**: <https://agents.md>. Released by OpenAI in
  2025 and since donated to the Linux Foundation's Agentic AI
  Foundation (secondary sources; the site was unreachable when this RFC
  was drafted).
