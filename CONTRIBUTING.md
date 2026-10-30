# Contributing to ourios

Thanks for your interest! ourios is design-first and pre-release.

## Dev setup
- Install Rust via `rust-toolchain.toml`, which pins an exact stable release (the
  MSRV is 1.94, set as `rust-version` in `Cargo.toml`).
- Install [`just`](https://github.com/casey/just) and run `just --list` to see tasks.

## AI coding agents
`AGENTS.md` is the project context for every agent, and `.agents/skills/` holds
the shared skills. The repository tracks nothing tool-specific. Codex, GitHub
Copilot, Cursor, Jules and Claude Code (v2.1.277 and later) read `AGENTS.md` as
is. To wire up the rest, run once per checkout, or as a cloud agent's setup
command:

```sh
sh .agents/setup.sh   # or: just agents-setup
```

It links each skill into `.claude/skills/` for Claude Code and writes
`.gemini/settings.json` so Gemini CLI loads `AGENTS.md`. Both are local and
gitignored, and rerunning the script is safe. If you keep a local `CLAUDE.md` or
`CLAUDE.local.md`, start it with `@AGENTS.md`; otherwise Claude Code reads it
instead of `AGENTS.md`. With such an import, a Claude Code session started below
the repository root prompts once to approve loading `AGENTS.md`.

## Before opening a PR
- `cargo fmt --all`
- `cargo clippy --all-targets --all-features -- -D warnings`
- `cargo test --all-features`
- `mdbook build` if you touched `docs/` (`just check` runs all four)
- For significant changes, open an RFC under `docs/rfcs/` first (see `docs/rfcs/README.md`).

## Commits & merging

We follow [Conventional Commits](https://www.conventionalcommits.org/) for the
history that lands on `main`, but we don't dictate how you craft your branch.

- **PR titles must be conventional.** The `PR Title` check (`lint`) is required
  on `main`. The title is what ends up on `main` if your PR is squashed.
- **Per-commit conventional messages are encouraged but not required.** The
  advisory `Commitlint` check reports commits that don't conform. It never
  blocks a merge.
- **Choose the merge strategy that fits the change.** `main` requires linear
  history, so merge commits are not possible:
  - **Squash** — default, especially if commits are messy or non-conventional.
  - **Rebase** — when each commit is meaningful and individually conventional.

Reviewers pick the strategy at merge time. When in doubt, squash.

## What `main` requires

The `main protection` ruleset requires:

- a pull request, with every review thread resolved (no approval count is set);
- the `CI Success` and `lint` (PR title) status checks, on a branch that is up
  to date with `main`;
- linear history (squash or rebase only);
- no force-pushes and no deletion of `main`.

## Signing commits

Signed commits are encouraged but the ruleset does not require them. Quick
setup with SSH:

```bash
git config --global gpg.format ssh
git config --global user.signingkey ~/.ssh/id_ed25519.pub
git config --global commit.gpgsign true
```

Add the same key to GitHub as a **Signing Key**:
https://github.com/settings/ssh/new

See: https://docs.github.com/authentication/managing-commit-signature-verification

## Developer Certificate of Origin (sign-off)

Every non-merge commit in a PR must be **signed off** with the [Developer
Certificate of Origin](https://developercertificate.org/) (DCO 1.1). This is
separate from the cryptographic signing above: signing proves *who* authored
the commit, sign-off certifies you have the *right* to submit it under the
project's license. Sign-off is required, and the `dco` workflow checks it on
every PR (merge commits and PRs opened by a bot actor — e.g. Renovate,
Dependabot — are exempt).

Add the trailer automatically with `-s`:

```bash
git commit -s -m "feat(scope): summary"
```

which appends a line matching your commit author:

```text
Signed-off-by: Your Name <you@example.com>
```

Missed it? `git commit --amend -s --no-edit` fixes the tip commit, or
`git rebase --signoff main` a whole range. If the branch is already pushed
(the usual case when a PR is open), this rewrites history, so follow with
`git push --force-with-lease`. AI-assisted commits keep their `Co-Authored-By:`
trailer *and* carry the human contributor's `Signed-off-by:` — the human
driver certifies origin.

## Conventions
- Keep PRs small and focused.
- Don't edit `CHANGELOG.md` in your PR. It is generated at release time: `just release`
  runs git-cliff over the conventional commit messages on `main`, so your commit
  message (or squashed PR title) is your changelog entry.

See `AGENTS.md` for architecture context and `CODE_OF_CONDUCT.md` for community expectations.
