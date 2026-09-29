# Every gate CI runs.
#
# These recipes exist so a commit can be checked exactly the way the pipeline
# will check it. CI reads the dependency list of `check` and runs each entry
# as a job of its own, in parallel, so a gate added here is a gate in CI.
#
# Each CI job spends about three minutes entering the Nix dev shell before its
# gate starts, so the entries are groups of gates rather than single ones.

# everything CI runs; what to run before committing
check: quick lint host-tests device-checks images

# the gates that compile nothing, so CI reports them first
quick: fmt deny lint-scripts lint-workflows machete

# the host test suite and the bench scripts' self-tests
host-tests: test scripts

# the library crates and the decoder against the device target
device-checks: cross link

# both firmware images
images: firmware firmware-release

# formatting, the gate no test or review will catch
#
# firmware/ and fuzz/ are workspaces of their own (see their Cargo.toml), so
# the root `--all` does not reach them and each is checked from inside, where
# cargo picks up firmware/.cargo/config.toml.
fmt:
    cargo fmt --all --check
    cd firmware && cargo fmt --all --check
    cd fuzz && cargo fmt --all --check

# Known advisories, licences and dependency sources, against the policy in
# deny.toml. Every workspace: the dependencies that ship are all in
# firmware/'s, and no copyleft code belongs in the tools either. firmware/ needs the vendored crates because its manifest
# patches them in.

# dependencies against the advisory database and the licence policy
deny: vendor
    cargo deny --manifest-path Cargo.toml --config deny.toml check
    cargo deny --manifest-path firmware/Cargo.toml --config deny.toml check
    cargo deny --manifest-path fuzz/Cargo.toml --config deny.toml check

# The bench scripts flash the box, record hours-long runs and parse the
# firmware's output, and no compiler checks them.

# shellcheck and ruff over scripts/
lint-scripts:
    shellcheck scripts/*.sh
    ruff check scripts/
    ruff format --check scripts/

# A mistake in a workflow shows only when GitHub runs it, which costs a push
# and billed minutes, and a broken expression can quietly skip a gate rather
# than fail it. A mistake in the Dependabot config shows only as an error in
# a tab nobody watches, while updates quietly stop.

# actionlint over .github/workflows/, and the Dependabot config's schema
lint-workflows:
    actionlint
    check-jsonschema --builtin-schema vendor.dependabot .github/dependabot.yml

# A dependency nothing uses still costs build time, and in firmware/ it can
# cost flash. cargo-machete reads source rather than compiling it, so it
# covers both workspaces from here in seconds; a dependency listed only for
# its features is named in that crate's `[package.metadata.cargo-machete]`.

# dependencies declared but not used, in both workspaces
machete:
    cargo machete

lint: vendor
    cargo clippy --workspace --all-targets -- -D warnings
    # No --all-targets here: xtensa-esp32s3-none-elf is bare-metal and has
    # no `test` crate, so building a test harness for it fails outright.
    # There is nothing under cfg(test) in firmware/ to lint anyway.
    cd firmware && cargo clippy --workspace -- -D warnings
    # The fuzz targets call the parsers' public API, so a change there must
    # not leave them unbuildable until the next time someone fuzzes.
    cd fuzz && cargo clippy -- -D warnings

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

# Both image recipes end in scripts/check-budget.sh: the stack region against
# the deepest use measured on the box, and the image against its OTA slot.
# They build to the same path, so each checks its own image straight after
# building it.

# the device firmware compiles, links, and fits
firmware: vendor
    cd firmware && cargo build --release
    ./scripts/check-budget.sh

# `TEDDIEBOX_RELEASE` is off unless set, so `just firmware` and `just flash`
# build a development image. Setting it leaves `dl` as the only console
# command (enough to flash a development build back on), which makes the image
# 18,848 bytes smaller. It works on any recipe that builds firmware, so a
# release flash is `TEDDIEBOX_RELEASE=1 just flash`; this recipe exists so CI
# builds that configuration too.

# the same image without the bench console commands
firmware-release: vendor
    cd firmware && TEDDIEBOX_RELEASE=1 cargo build --release
    ./scripts/check-budget.sh

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

# writes an over-the-air update to target/ota/: the image and its manifest
#
# Publish both into the directory `update_url` names. The manifest's version
# is read out of the image, so the two always agree.
ota-image: firmware
    ./scripts/ota-image.sh

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

# The parsers read what a card, a server or any device on the portal's open
# access point sends. The property tests check what their generators aim
# at; the fuzzer searches for what they missed, guided by coverage. Not a
# gate: a run finds different inputs each time, so it cannot give a commit a
# verdict. A crash or a hang is saved under fuzz/artifacts/<target>/ and
# replays with `cargo fuzz run -s none <target> <file>`.
#
# No sanitizer: the esp toolchain ships none, and the code under test is safe
# Rust, where a panic, an overflow or a hang is what there is to find.
# `-timeout` turns an input that takes longer than 10 s into a failure, so a
# parser that loops is reported instead of running out the clock.
#
# Random bytes rarely get past a parser's first check, so each target starts
# from well-formed inputs in fuzz/seeds/ (the TAF reader from the committed
# fixtures) and mutates with the format's tokens from fuzz/dict/. Without
# them, four minutes never produced a response with two `1xx` preambles.

# fuzz every parser, or one: `just fuzz`, `just fuzz taf_reader 600`
fuzz target="" seconds="60":
    #!/usr/bin/env bash
    set -euo pipefail
    targets="{{target}}"
    [ -n "$targets" ] || targets="$(cargo fuzz list)"
    for t in $targets; do
        echo "--- $t, {{seconds}} s"
        seeds=()
        [ -d "fuzz/seeds/$t" ] && seeds=("fuzz/seeds/$t")
        [ "$t" = taf_reader ] && seeds=(crates/teddiebox-taf/tests/data)
        dict=()
        case "$t" in
            cloud_head | portal_http) dict=(-dict=fuzz/dict/http.dict) ;;
            ota_* | portal_form) dict=(-dict=fuzz/dict/keyvalue.dict) ;;
            taf_*) dict=(-dict=fuzz/dict/taf.dict) ;;
        esac
        mkdir -p "fuzz/corpus/$t"
        cargo fuzz run -s none "$t" "fuzz/corpus/$t" "${seeds[@]}" -- \
            -max_total_time={{seconds}} -timeout=10 "${dict[@]}"
    done

# Mutation testing: cargo-mutants changes the code one small way at a time
# (a `<` for a `<=`, a body replaced by `Default::default()`) and reports
# each change no test notices. A missed mutant is a behaviour no test pins
# down, or code that does nothing. Not a gate: the whole workspace is some
# 1,900 mutants and hours of building, and a missed one needs judgement,
# not an automatic red. Settings are in .cargo/mutants.toml; results land in
# mutants.out/, with the missed ones in mutants.out/missed.txt. Set
# MUTANTS_JOBS to change how many run at once (each is a build).

# mutation testing: `just mutants`, or `just mutants -p teddiebox-taf`
mutants *args:
    cargo mutants --jobs "${MUTANTS_JOBS:-4}" {{args}}

# format the tree rather than checking it
#
# Every workspace, like `fmt`: formatting only the root would leave
# `just check` failing on firmware/ or fuzz/.
fix:
    cargo fmt --all
    cd firmware && cargo fmt --all
    cd fuzz && cargo fmt --all

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
