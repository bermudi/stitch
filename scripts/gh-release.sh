#!/usr/bin/env bash
# Publish the GitHub release for an already-tagged stitch version.
# Notes come from the CHANGELOG section for that version.
#
# Usage: scripts/gh-release.sh v0.8.0 "encrypted secrets"
#
# Preconditions: the annotated tag vX.Y.Z exists (release commits are always
# tagged) and the CHANGELOG has a "## X.Y.Z — <date>" section.
set -euo pipefail

ver="${1:-}"
summary="${2:-}"
if [ -z "$ver" ] || [ -z "$summary" ]; then
    echo "usage: scripts/gh-release.sh vX.Y.Z \"one-line summary\"" >&2
    exit 2
fi

if ! git rev-parse --verify --end-of-options "refs/tags/$ver" >/dev/null 2>&1; then
    echo "error: tag $ver does not exist — tag the release commit first" >&2
    exit 1
fi

# Extract the CHANGELOG section for this version: everything after the
# "## X.Y.Z — <date>" header up to the next "## " header. The header line
# itself is dropped; the release title carries the version + summary.
notes="$(mktemp)"
trap 'trash -f "$notes"' EXIT
awk -v v="${ver#v}" '
    /^## / {
        if (insec) exit
        insec = ($2 == v)
        next
    }
    insec { print }
' CHANGELOG.md > "$notes"
# Command substitution strips the trailing newlines (incl. the blank line
# before the next section header); printf adds back exactly one.
printf '%s\n' "$(cat "$notes")" > "$notes"

[ -s "$notes" ] || {
    echo "error: no CHANGELOG section for $ver (expected \"## ${ver#v} — <date>\")" >&2
    exit 1
}

gh release create "$ver" --title "$ver — $summary" --notes-file "$notes"
