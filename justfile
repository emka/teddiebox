# Every gate CI runs, in the order it runs them.
#
# These recipes exist so a commit can be checked exactly the way the pipeline
# will check it. If a gate is added to .github/workflows/ci.yml it belongs here
# too, or the two drift and this stops being worth running.

# everything CI runs; what to run before committing
check: fmt lint test cross link fixtures firmware

# formatting, the gate no test or review will catch
#
# firmware/ is its own workspace (see firmware/Cargo.toml), so it is not
# reached by the root `--all` and must be checked separately, from inside
# firmware/ so cargo picks up firmware/.cargo/config.toml.
fmt:
    cargo fmt --all --check
    cd firmware && cargo fmt --all --check

lint:
    cargo clippy --workspace --all-targets -- -D warnings
    # No --all-targets here: xtensa-esp32s3-none-elf is bare-metal and has
    # no `test` crate, so building a test harness for it fails outright.
    # There is nothing under cfg(test) in firmware/ to lint anyway.
    cd firmware && cargo clippy --workspace -- -D warnings

test:
    cargo test --workspace

# Proves the library crates are genuinely no_std, against the target the
# firmware actually runs on. rustc ships no prebuilt core for xtensa.

# no_std check against the device target
cross:
    #!/usr/bin/env bash
    set -euo pipefail
    for crate in teddiebox-taf teddiebox-core teddiebox-config teddiebox-cloud \
                 teddiebox-download tlv320dac3100 trf7962a lis3dh; do
        echo "--- $crate"
        cargo check -p "$crate" --target xtensa-esp32s3-none-elf -Z build-std=core
    done

# Catches an FFI mismatch or a libopus dependency the device's C library
# cannot satisfy, neither of which a cross-compile check can see.

# link the decode path into a real image
link:
    ./scripts/xtensa-link-check.sh

# The committed fixtures are the evidence base for every format claim in the
# parser, so a generator that has drifted from them invalidates it quietly.

# committed fixtures still match their generator
fixtures:
    #!/usr/bin/env bash
    set -euo pipefail
    out="$(mktemp -d)"
    trap 'rm -rf "$out"' EXIT
    cargo run -q -p fixturegen -- "$out/sine.taf"
    diff "$out/sine.taf" crates/teddiebox-taf/tests/data/sine.taf
    diff "$out/chapters.taf" crates/teddiebox-taf/tests/data/chapters.taf

# the device firmware compiles and links for the target
firmware:
    cd firmware && cargo build --release

# format the tree rather than checking it
fix:
    cargo fmt --all
