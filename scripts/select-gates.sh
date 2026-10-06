#!/usr/bin/env bash
# Reads the changed file names on stdin and the JSON list of every CI gate as
# the first argument, and prints the JSON list of gates the change needs.
#
# A change confined to files no compiler, test or linter but `quick` reads
# needs only `quick`. Anything else, an empty change, or a list without
# `quick` needs every gate: a wrong skip hides a break, a wrong run costs
# minutes.
set -eu

all=$1
needs_all=false
seen=false

while IFS= read -r file; do
    [ -n "$file" ] || continue
    seen=true
    case "$file" in
        .github/workflows/ci.yml) needs_all=true ;;
        .github/workflows/* | .github/dependabot.yml | docs/*) ;;
        */*) needs_all=true ;;
        *.md | LICENSE-*) ;;
        *) needs_all=true ;;
    esac
done

quick=$(jq -c 'map(select(. == "quick"))' <<<"$all")
if [ "$seen" = false ] || [ "$needs_all" = true ] || [ "$quick" = '[]' ]; then
    jq -c . <<<"$all"
else
    echo "$quick"
fi
