# Contributing to ourios

Thanks for your interest! ourios is design-first and pre-release.

## Dev setup
- Install Rust via the pinned `rust-toolchain.toml`.
- Install [`just`](https://github.com/casey/just) and run `just --list` to see tasks.

## Before opening a PR
- `cargo fmt --all`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test --workspace`
- For significant changes, open an RFC under `docs/rfcs/` first (see `docs/rfcs/README.md`).

## Commits & merging

We follow [Conventional Commits](https://www.conventionalcommits.org/) for the
history that lands on `main`, but we don't dictate how you craft your branch.

- **PR titles must be conventional** (enforced by CI). The title is what ends
  up on `main` if your PR is squashed.
- **Per-commit conventional messages are encouraged but not required.** A bot
  will comment if your commits don't conform — that's a hint to the reviewer,
  not a blocker.
- **Choose the merge strategy that fits the change:**
  - **Squash** — default, especially if commits are messy or non-conventional.
  - **Rebase** — when each commit is meaningful and individually conventional.
  - **Merge commit** — for landing larger features (e.g. an RFC implementation)
    where preserving the branch shape matters.

Reviewers pick the strategy at merge time. When in doubt, squash.

## Signed commits required

`main` requires every commit to be signed and verified. Quick setup with SSH:

```bash
git config --global gpg.format ssh
git config --global user.signingkey ~/.ssh/id_ed25519.pub
git config --global commit.gpgsign true
```

Add the same key to GitHub as a **Signing Key**:
https://github.com/settings/ssh/new

See: https://docs.github.com/authentication/managing-commit-signature-verification

## Conventions
- Keep PRs small and focused.
- Update `CHANGELOG.md` under `## [Unreleased]`.

See `CLAUDE.md` for architecture context and `CODE_OF_CONDUCT.md` for community expectations.
