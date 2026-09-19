# Every gate CI runs, in the order it runs them.
#
# These recipes exist so a commit can be checked exactly the way the pipeline
# will check it. If a gate is added to .github/workflows/ci.yml it belongs here
# too, or the two drift and this stops being worth running.

# everything CI runs; what to run before committing
check: fmt lint test scripts cross link fixtures firmware firmware-release

# formatting, the gate no test or review will catch
#
# firmware/ is its own workspace (see firmware/Cargo.toml), so it is not
# reached by the root `--all` and must be checked separately, from inside
# firmware/ so cargo picks up firmware/.cargo/config.toml.
fmt:
    cargo fmt --all --check
    cd firmware && cargo fmt --all --check

lint: vendor
    cargo clippy --workspace --all-targets -- -D warnings
    # No --all-targets here: xtensa-esp32s3-none-elf is bare-metal and has
    # no `test` crate, so building a test harness for it fails outright.
    # There is nothing under cfg(test) in firmware/ to lint anyway.
    cd firmware && cargo clippy --workspace -- -D warnings

test:
    cargo test --workspace

# The bench scripts' own self-tests
#
# These scripts decide what a bench session records, and two of them parse the
# firmware's output. A parser nothing exercises is a parser that quietly stops
# matching after a print statement is reworded — and the session that finds out
# is one somebody drove to the bench for.
scripts:
    python3 scripts/battery-run.py --self-test
    python3 scripts/sleep-check.py --self-test

# Proves the library crates are genuinely no_std, against the target the
# firmware actually runs on. rustc ships no prebuilt core for xtensa.

# no_std check against the device target
cross:
    #!/usr/bin/env bash
    set -euo pipefail
    for crate in teddiebox-taf teddiebox-core teddiebox-config teddiebox-cloud \
                 teddiebox-download teddiebox-ota teddiebox-portal \
                 teddiebox-board teddiebox-console \
                 tlv320dac3100 trf7962a lis3dh; do
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
firmware: vendor
    cd firmware && cargo build --release

# `TEDDIEBOX_RELEASE` is off unless set, so `just firmware` and `just flash`
# build the image a bench session wants. Setting it leaves `dl` as the only
# console command in the image — enough to flash a bench build back on, and
# nothing else — which is 18,848 bytes smaller. It works on any recipe that
# builds firmware, so a release flash is `TEDDIEBOX_RELEASE=1 just flash`; this
# recipe exists so CI builds that configuration too, because one nothing builds
# is one that rots.

# the same image without the bench console commands
firmware-release: vendor
    cd firmware && TEDDIEBOX_RELEASE=1 cargo build --release

# firmware/Cargo.toml patches both mbedtls crates to copies of the published
# ones — `mbedtls-rs-sys` for a widened version bound and a define it will not
# let go of, `mbedtls-rs` for a call it never makes — and cargo cannot even
# parse the manifest until those copies exist. Every recipe that builds
# firmware/ depends on this; it is a no-op once they are there. The why is
# beside each patch entry in firmware/Cargo.toml and at the head of each script.

# fetch and patch the vendored mbedtls crates
vendor:
    ./scripts/vendor-mbedtls-rs-sys.sh
    ./scripts/vendor-mbedtls-rs.sh

# put the box in download mode, flash it, and start it again
#
# The order in scripts/flash.sh is not arbitrary: this board has no wired
# reset, espflash must touch the port before esptool ever does, and esptool
# must not run at all if espflash failed. Getting any of those wrong costs a
# J100 cold boot with the case open, which is why it is a recipe and not
# something to retype.
flash: firmware
    ./scripts/flash.sh

# format the tree rather than checking it
fix:
    cargo fmt --all
