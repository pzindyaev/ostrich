#!/usr/bin/env bash
# Writes the release notes for a tag from the git history since the previous
# tag, grouped like the old GoReleaser configuration: Features, Bug fixes,
# Other changes; chore/docs/test/ci commits and typo fixes left out.
#
#   scripts/changelog.sh <tag>
#
# Privacy: only the short SHA and the subject of each commit appear — never
# an author name, e-mail or handle.
set -euo pipefail

tag=$1
prev=$(git describe --tags --abbrev=0 "$tag^" 2>/dev/null || true)
range=${prev:+$prev..}$tag

mapfile -t lines < <(git log --format='%h: %s' "$range" \
  | grep -Ev '^[0-9a-f]+: (chore|docs|test|ci)(\([^)]*\))?!?:' \
  | grep -Eiv 'typo' || true)

features=() fixes=() others=()
for l in "${lines[@]}"; do
  subject=${l#*: }
  if [[ $subject =~ ^feat(ure)?(\([[:alnum:]_-]+\))?!?: ]]; then
    features+=("$l")
  elif [[ $subject =~ ^fix(\([[:alnum:]_-]+\))?!?: ]]; then
    fixes+=("$l")
  else
    others+=("$l")
  fi
done

echo "## Changelog"
section() {
  local title=$1; shift
  [[ $# -eq 0 ]] && return
  echo
  echo "### $title"
  for l in "$@"; do echo "* $l"; done
}
section "Features" "${features[@]}"
section "Bug fixes" "${fixes[@]}"
section "Other changes" "${others[@]}"
