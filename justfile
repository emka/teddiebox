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

test: vendor
    cargo test --workspace
    # The one change carried in the vendored smoltcp, under smoltcp's own
    # tests, so the patch cannot drift from what it claims.
    cargo test --quiet --manifest-path firmware/vendor/smoltcp/Cargo.toml --target-dir target/vendor-smoltcp --lib iface::neighbor

# The bench scripts' own self-tests
#
# Two of these scripts parse the firmware's output. Without tests, a parser
# quietly stops matching when a print statement is reworded.
scripts:
    python3 scripts/battery-run.py --self-test
    python3 scripts/sleep-check.py --self-test
    python3 scripts/console.py --self-test
    python3 scripts/bench-console.py --self-test

# Proves the library crates are genuinely no_std, against the target the
# firmware actually runs on. rustc ships no prebuilt core for xtensa.

# no_std check against the device target
cross:
    #!/usr/bin/env bash
    set -euo pipefail
    for crate in teddiebox-taf teddiebox-core teddiebox-config teddiebox-cloud \
                 teddiebox-download teddiebox-identity teddiebox-ota teddiebox-portal \
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
# build a development image. Setting it leaves `dl` as the only console
# command (enough to flash a development build back on), which makes the image
# 18,848 bytes smaller. It works on any recipe that builds firmware, so a
# release flash is `TEDDIEBOX_RELEASE=1 just flash`; this recipe exists so CI
# builds that configuration too.

# the same image without the bench console commands
firmware-release: vendor
    cd firmware && TEDDIEBOX_RELEASE=1 cargo build --release

# firmware/Cargo.toml replaces three crates with patched copies of the
# published ones (`mbedtls-rs-sys`, `mbedtls-rs` and `smoltcp`), and cargo
# cannot parse the manifest until those copies exist. Every recipe that
# builds firmware/ depends on this; it does nothing once they exist. The
# reasons are beside each patch entry in firmware/Cargo.toml and at the top
# of each script.

# fetch and patch the vendored crates
vendor:
    ./scripts/vendor-mbedtls-rs-sys.sh
    ./scripts/vendor-mbedtls-rs.sh
    ./scripts/vendor-smoltcp.sh

# put the box in download mode, flash it, and start it again
#
# The order in scripts/flash.sh is not arbitrary: this board has no wired
# reset, espflash must touch the port before esptool ever does, and esptool
# must not run at all if espflash failed. Getting any of those wrong means
# opening the case to short J100 and cold-boot the box.
flash: firmware
    ./scripts/flash.sh

# writes the box's TLS identity into the `cert` partition
#
# Separate from `just flash` deliberately: an app write never touches a data
# partition, so this is run once per box and survives every later firmware
# flash. So a fresh box can play its card but cannot download until this is
# run, which it reports at boot.
#
# Needs TEDDIEBOX_IDENTITY_DIR, set in .envrc.local. The offset must match
# `cert` in partitions.csv.
identity:
    #!/usr/bin/env bash
    set -euo pipefail
    out="$(mktemp -d)"
    trap 'rm -rf "$out"' EXIT
    cargo run -q -p identity-image -- "$out/identity.bin"
    # Read from the table rather than repeated here, so moving the partition
    # cannot leave this writing the private key to the old address.
    addr="$(awk -F', *' '/^cert,/ { print $4 }' partitions.csv)"
    [ -n "$addr" ] || { echo "just identity: no cert partition in partitions.csv" >&2; exit 1; }
    echo "identity: writing to $addr, per partitions.csv"
    BIN_FILE="$out/identity.bin" BIN_ADDR="$addr" ./scripts/flash.sh

# `scripts/bench-console.py` sends one line and captures until a marker, for
# scripts. This is for interactive use, such as plugging in headphones between
# `t` and `stop`. Only one program may use the port, so this refuses to start
# while a capture is running, and `just flash` refuses while this is.

# an interactive console on the box, for what a capture cannot do
console:
    ./scripts/console.py

# format the tree rather than checking it
#
# Both workspaces, like `fmt`: formatting only the root would leave
# `just check` failing on firmware/.
fix:
    cargo fmt --all
    cd firmware && cargo fmt --all

# Not a CI gate: there is no agreed complexity budget to fail a build against,
# so this stays a feedback tool, not an enforced one. Per-file complexity
# (scc) finds which files carry more complexity than their size accounts for;
# per-function complexity (complexity-report.py) finds which functions inside
# a flagged file it's actually concentrated in.

# size and complexity report: `just complexity` for a summary, or
# `just complexity path/to/file.rs` for per-function cyclomatic/cognitive
# complexity in one file
complexity file="":
    #!/usr/bin/env bash
    set -euo pipefail
    if [ -z "{{file}}" ]; then
        tokei firmware/src crates
        echo
        scc firmware/src crates --by-file -s complexity -n 20
    else
        rust-code-analysis-cli -p "{{file}}" -m -O json \
            | python3 scripts/complexity-report.py
    fi
