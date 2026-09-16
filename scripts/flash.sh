#!/usr/bin/env bash
#
# Put the box in download mode, flash it, and start it again.
#
# This exists because the order matters and getting it wrong costs a J100 cold
# boot with the board opened up. Every rule below was learned that way; see
# HARDWARE.md for the long version.
#
#   1. DTR and RTS are not wired on this board, so espflash's and esptool's
#      auto-reset cannot work. Every invocation needs --before no-reset.
#   2. espflash must be the *first* tool to touch the port after the box enters
#      download mode. Running esptool first — even flash-id — leaves its stub
#      loader resident and espflash then cannot connect.
#   3. esptool runs only if espflash succeeded. Piping espflash through `tail`
#      hides its exit status behind tail's, which is how a failed flash was
#      once followed by an esptool run that wedged the box.
#   4. The port takes exactly one owner, so a capture still holding it makes
#      the flash fail in a way that looks like the box is dead.
#   5. The partition table is ours, not espflash's default: the default has a
#      single `factory` app and no `otadata`, which cannot be updated over the
#      air. --flash-size is stated rather than detected because the same table
#      is refused against espflash's offline 4 MB default, and that failure
#      reads as a bad table rather than a missing flag.

set -euo pipefail

PORT="${PORT:-/dev/ttyUSB0}"
ELF="${ELF:-firmware/target/xtensa-esp32s3-none-elf/release/teddiebox-firmware}"
TABLE="${TABLE:-partitions.csv}"

die() {
    echo "flash: $*" >&2
    exit 1
}

[ -f "$ELF" ] || die "no firmware at $ELF — run 'just firmware' first"
[ -f "$TABLE" ] || die "no partition table at $TABLE — run this from the repository root"
[ -e "$PORT" ] || die "no $PORT — is the box plugged in?"

if pgrep -f "bench-console.*$PORT" >/dev/null 2>&1; then
    die "something is already reading $PORT; the port takes one owner — stop it first"
fi

# A box already sitting in download mode is silent, and asking it for `dl`
# just times out. SKIP_DL=1 goes straight to flashing.
if [ "${SKIP_DL:-0}" = "1" ]; then
    echo "flash: assuming the box is already in download mode"
else

# Ask the firmware to reboot into the ROM's download mode. This is the only
# software route in, and it needs the console to be listening: a box whose
# console has stopped reading, or which crashes before the console starts, can
# only be recovered by shorting J100 and applying power cold.
echo "flash: asking the box to enter download mode"
capture=$(mktemp)
trap 'rm -f "$capture"' EXIT
# Three seconds, not two. The console is up long before that, but a box that
# is mid-request answers late, and a `dl` sent too early is simply dropped —
# which reads exactly like a box that has stopped listening, and cost a
# misdiagnosis and very nearly a needless J100.
python3 scripts/bench-console.py --port "$PORT" --send dl --send-after 3.0 \
    --until "waiting for download" --timeout 25 --out "$capture" >/dev/null 2>&1 || true

if ! grep -q "waiting for download" "$capture"; then
    if [ -s "$capture" ]; then
        die "the box is talking but did not accept 'dl'.
     Try once more before assuming the worst: a box busy with a request answers
     late, and that looks identical to one that has stopped listening. Send it
     any command by hand — if it answers, its console is fine and this was
     impatience.
     If it truly ignores everything, short J100 and apply power cold.
     NOT running esptool either way: after a failed entry it wedges the box."
    fi
    die "no answer from the box on $PORT.
     If it is already in download mode this is expected — rerun with SKIP_DL=1.
     Otherwise short J100 and apply power cold."
fi

fi

echo "flash: writing $ELF"
# espflash first, and its status captured directly rather than through a pipe.
if ! espflash flash --port "$PORT" --before no-reset --after no-reset \
    --flash-size 8mb --partition-table "$TABLE" \
    -B 921600 --non-interactive "$ELF"; then
    die "espflash failed.
     NOT running esptool — after a failed flash it leaves the box needing a
     J100 cold boot. Put the box back in download mode and try again."
fi

echo "flash: starting the firmware"
esptool --port "$PORT" --before no-reset --after watchdog-reset run >/dev/null

# esptool leaves the line in a state the console reader does not expect.
stty -F "$PORT" 115200 raw -echo
echo "flash: done"
