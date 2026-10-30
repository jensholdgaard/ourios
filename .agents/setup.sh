#!/bin/sh
# setup.sh — create the local, untracked bridges that let agents which do not
# read AGENTS.md or .agents/skills/ natively find them (RFC 0057 §3.8).
#
# The repository tracks only AGENTS.md and .agents/; everything this script
# writes is gitignored. It is idempotent and never replaces a file it did not
# create, so it is safe to rerun and to use as a cloud agent's setup command:
#
#   sh .agents/setup.sh
set -eu

cd "$(dirname "$0")/.."

say() { printf 'agents-setup: %s\n' "$*"; }

# Claude Code discovers project skills only under .claude/skills/.
mkdir -p .claude/skills
for skill in .agents/skills/*/; do
  name=$(basename "$skill")
  target="../../.agents/skills/$name"
  link=".claude/skills/$name"
  if [ -L "$link" ]; then
    [ "$(readlink "$link")" = "$target" ] || say "$link points elsewhere; left as is"
  elif [ -e "$link" ]; then
    say "$link exists and is not a link; left as is"
  else
    ln -s "$target" "$link"
    say "linked $link"
  fi
done
for link in .claude/skills/*; do
  if [ -L "$link" ] && [ ! -e "$link" ]; then
    case $(readlink "$link") in
      ../../.agents/skills/*) rm "$link" && say "removed stale $link" ;;
    esac
  fi
done

# Gemini CLI reads AGENTS.md only when context.fileName lists it. GEMINI.md
# stays in the list: the project value replaces the user one, and it also
# names the global context file (~/.gemini/GEMINI.md).
if [ ! -e .gemini/settings.json ]; then
  mkdir -p .gemini
  printf '{"context":{"fileName":["AGENTS.md","GEMINI.md"]}}\n' > .gemini/settings.json
  say "wrote .gemini/settings.json"
elif ! grep -q 'AGENTS\.md' .gemini/settings.json; then
  say ".gemini/settings.json does not list AGENTS.md in context.fileName; add it"
fi

# A local CLAUDE.md or CLAUDE.local.md makes Claude Code read it instead of
# AGENTS.md, unless it imports AGENTS.md.
for file in CLAUDE.md CLAUDE.local.md; do
  if [ -f "$file" ] && ! grep -qx '@AGENTS.md' "$file"; then
    say "$file does not import AGENTS.md; add a line '@AGENTS.md', or Claude Code reads it instead of AGENTS.md"
  fi
done
