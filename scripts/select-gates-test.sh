#!/usr/bin/env bash
# Behaviour of select-gates.sh: which CI gates a set of changed files needs.
set -u
cd "$(dirname "$0")" || exit 1

FULL='["quick","lint","host-tests","device-checks","images"]'
QUICK='["quick"]'
failed=0

# selected <expected> <description> <changed files...>
selected() {
    local expected=$1 description=$2 actual
    shift 2
    actual=$(printf '%s\n' "$@" | ./select-gates.sh "$FULL")
    if [ "$actual" != "$expected" ]; then
        echo "FAIL: $description: expected $expected, got $actual"
        failed=1
    fi
}

# Given / When / Then in each call: the files are the given, the call is the
# when, the expected list is the then.
selected "$QUICK" "a root markdown file needs only quick" README.md
selected "$QUICK" "documents need only quick" docs/public-checklist.md
selected "$QUICK" "another workflow needs only quick" .github/workflows/release-please.yml
selected "$QUICK" "the dependabot config needs only quick" .github/dependabot.yml
selected "$QUICK" "a licence file needs only quick" LICENSE-MIT
selected "$QUICK" "several unrelated files need only quick" README.md CHANGELOG.md docs/a.md
selected "$FULL" "the pipeline itself needs every gate" .github/workflows/ci.yml
selected "$FULL" "source needs every gate" crates/teddiebox-core/src/lib.rs
selected "$FULL" "a markdown file inside a crate needs every gate" crates/teddiebox-core/README.md
selected "$FULL" "the justfile needs every gate" justfile
selected "$FULL" "a lockfile needs every gate" firmware/Cargo.lock
selected "$FULL" "one related file among unrelated ones needs every gate" README.md crates/trf7962a/src/lib.rs
selected "$FULL" "no changed files needs every gate"

# Given a gate list without quick, When only unrelated files changed,
# Then nothing can be skipped safely.
actual=$(printf 'README.md\n' | ./select-gates.sh '["lint","images"]')
if [ "$actual" != '["lint","images"]' ]; then
    echo "FAIL: a list without quick runs in full: got $actual"
    failed=1
fi

[ "$failed" = 0 ] && echo "select-gates: all passed"
exit "$failed"
