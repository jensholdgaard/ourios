#!/usr/bin/env bash
# check-agents-citations.sh — fail when a tracked file cites an `AGENTS.md §N`
# or `CLAUDE.md §N` section that AGENTS.md no longer has (RFC0057.4).
#
# AGENTS.md section numbers are frozen (RFC 0057 §3.1) and hundreds of files
# cite them; historical RFCs still use the old `CLAUDE.md §N` spelling. A
# citation resolves to an `## N.` / `### N.M` heading, or to item M of the
# numbered list in §2 (pillars) or §4 (hazards), e.g. `§2.1`, `§4.6`.
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

valid=$(awk '
  /^## [0-9]+\./ { sub(/^## /, ""); sub(/\..*/, ""); section = $0; print section; next }
  /^### [0-9]+\.[0-9]+ / { print $2; next }
  (section == "2" || section == "4") && /^[0-9]+\. / { sub(/\..*/, ""); print section "." $0 }
' AGENTS.md | sort -u)

broken=0
while IFS=: read -r file line citation; do
  number=$(sed -E 's/.*§ ?([0-9]+(\.[0-9]+)*).*/\1/' <<<"$citation")
  if ! grep -qxF "$number" <<<"$valid"; then
    echo "::error file=${file},line=${line}::cites §${number}, which AGENTS.md does not have"
    broken=1
  fi
done < <(git grep -noE '(CLAUDE|AGENTS)\.md.? ?§ ?[0-9]+(\.[0-9]+)*' || true)

if [ "$broken" -ne 0 ]; then
  echo "AGENTS.md section numbers are frozen (RFC 0057 §3.1): fix the citation or restore the section." >&2
  exit 1
fi
echo "All AGENTS.md / CLAUDE.md section citations resolve."
