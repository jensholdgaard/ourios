#!/usr/bin/env bash
# check-agents-citations.sh — fail when a tracked file cites an `AGENTS.md §N`
# or `CLAUDE.md §N` section that AGENTS.md no longer has (RFC0057.4).
#
# AGENTS.md section numbers are frozen (RFC 0057 §3.1) and hundreds of files
# cite them; historical RFCs still use the old `CLAUDE.md §N` spelling. A
# citation resolves to an `## N.` / `### N.M` heading, or to item M of the
# numbered list in §2 (pillars) or §4 (hazards), e.g. `§2.1`, `§4.6`. A
# citation may wrap: `AGENTS.md` ends one line and `§N` opens the next, after
# any comment marker.
set -euo pipefail
export LC_ALL=C

cd "$(git rev-parse --show-toplevel)"

valid=$(awk '
  /^## [0-9]+\./ { sub(/^## /, ""); sub(/\..*/, ""); section = $0; print section; next }
  /^### [0-9]+\.[0-9]+ / { print $2; next }
  (section == "2" || section == "4") && /^[0-9]+\. / { sub(/\..*/, ""); print section "." $0 }
' AGENTS.md | sort -u)

# Prints file:line:number for every citation.
# shellcheck disable=SC2016 # the backticks are awk regex text, not expansions
citations() {
  git grep -lzE '(CLAUDE|AGENTS)\.md' | xargs -0 awk '
    function emit(cited) { sub(/^.*§ ?/, "", cited); print FILENAME ":" FNR ":" cited }
    FNR == 1 { wrapped = 0 }
    {
      if (wrapped) {
        head = $0
        sub(/^[ \t]*((\/\/[\/!]?|#|\*|>|--)[ \t]*)?[`(]*/, "", head)
        if (match(head, /^§ ?[0-9]+(\.[0-9]+)*/)) emit(substr(head, RSTART, RLENGTH))
      }
      rest = $0
      while (match(rest, /(CLAUDE|AGENTS)\.md.? ?§ ?[0-9]+(\.[0-9]+)*/)) {
        emit(substr(rest, RSTART, RLENGTH))
        rest = substr(rest, RSTART + RLENGTH)
      }
      wrapped = ($0 ~ /(CLAUDE|AGENTS)\.md[]`*)]*[ \t]*$/)
    }
  '
}

broken=0
while IFS=: read -r file line number; do
  if ! grep -qxF "$number" <<<"$valid"; then
    echo "::error file=${file},line=${line}::cites §${number}, which AGENTS.md does not have"
    broken=1
  fi
done < <(citations)

if [ "$broken" -ne 0 ]; then
  echo "AGENTS.md section numbers are frozen (RFC 0057 §3.1): fix the citation or restore the section." >&2
  exit 1
fi
echo "All AGENTS.md / CLAUDE.md section citations resolve."
