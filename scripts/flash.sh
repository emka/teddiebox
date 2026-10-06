#!/usr/bin/env bash
#
# Put the box in download mode, flash it, and start it again.
#
# The order matters: getting it wrong means opening the box to short J100 and
# cold-boot it. See HARDWARE.md for details.
#
#   1. DTR and RTS are not wired on this board, so espflash's and esptool's
#      auto-reset cannot work. Every invocation needs --before no-reset.
#   2. espflash must be the *first* tool to touch the port after the box enters
#      download mode. Running esptool first — even flash-id — leaves its stub
#      loader resident and espflash then cannot connect.
#   3. esptool runs only if espflash succeeded; esptool after a failed
#      espflash hangs the box. So espflash is never piped (e.g. through
#      `tail`), which would hide its exit status.
#   4. The port takes exactly one owner, so a capture still holding it makes
#      the flash fail in a way that looks like the box is dead.
#   5. The partition table is the stock Toniebox one, at 0x9000, and needs the
#      stock bootloader: espflash's bundled bootloader is built for 0x8000 and
#      would not find it. The bootloader is cut from the box's own dump
#      (TEDDIEBOX_STOCK_DUMP), so the manufacturer's binary is never kept in the
#      repository. --flash-size is stated rather than detected because the same
#      table is refused against espflash's offline 4 MB default, and that
#      failure reads as a bad table rather than a missing flag.
#   6. espflash writes the app into ota_0, but the bootloader boots whichever
#      slot `otadata` selects. After an over-the-air update that is ota_1, so
#      the flashed image would never run. Erasing `otadata` makes the
#      bootloader fall back to ota_0.

set -euo pipefail

PORT="${PORT:-/dev/ttyUSB0}"
ELF="${ELF:-firmware/target/xtensa-esp32s3-none-elf/release/teddiebox-firmware}"
TABLE="${TABLE:-partitions.csv}"
# Must match the firmware's ESP_BOOTLOADER_ESP_IDF_CONFIG_PARTITION_TABLE_OFFSET
# (firmware/.cargo/config.toml) and the bootloader in the dump.
TABLE_OFFSET=0x9000
# The stock bootloader is at the start of the dump; this is far more than it
# needs, and the rest is blank flash.
BOOTLOADER_BYTES=32768
STOCK_DUMP="${TEDDIEBOX_STOCK_DUMP:-}"

# Writing a binary at an offset instead of the firmware: same port rules, same
# download-mode entry, same "esptool only after espflash succeeded" ordering.
# `just identity` is the only caller.
BIN_FILE="${BIN_FILE:-}"
BIN_ADDR="${BIN_ADDR:-}"

die() {
    echo "flash: $*" >&2
    exit 1
}

if [ -n "$BIN_FILE" ]; then
    [ -n "$BIN_ADDR" ] || die "BIN_FILE needs BIN_ADDR"
    [ -f "$BIN_FILE" ] || die "no binary at $BIN_FILE"
else
    [ -f "$ELF" ] || die "no firmware at $ELF — run 'just firmware' first"
    [ -n "$STOCK_DUMP" ] || die "TEDDIEBOX_STOCK_DUMP is not set — it names the flash dump
     of a stock box, which the stock bootloader is cut from (see README)"
    [ -f "$STOCK_DUMP" ] || die "no flash dump at $STOCK_DUMP"
fi
[ -f "$TABLE" ] || die "no partition table at $TABLE — run this from the repository root"
[ -e "$PORT" ] || die "no $PORT — is the box plugged in?"

# console.py finds who holds the port, from /proc. Matching command lines
# would miss a reader using the default port, which is not on its command
# line.
if holder=$(python3 scripts/console.py --check-owner --port "$PORT"); then
    :
else
    die "something is already reading $PORT; the port takes one owner — stop it first:
  $holder"
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
# Three seconds, not two: a box busy with a request answers late, and a `dl`
# sent too early is dropped, which looks like a box that stopped listening.
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

if [ -n "$BIN_FILE" ]; then
    echo "flash: writing $BIN_FILE at $BIN_ADDR"
    if ! espflash write-bin --port "$PORT" --before no-reset --after no-reset \
        -B 921600 --non-interactive "$BIN_ADDR" "$BIN_FILE"; then
        die "espflash failed.
     NOT running esptool — after a failed write it leaves the box needing a
     J100 cold boot. Put the box back in download mode and try again."
    fi
else
    echo "flash: writing $ELF"
    bootloader=$(mktemp)
    trap 'rm -f "${capture:-}" "$bootloader"' EXIT
    head -c "$BOOTLOADER_BYTES" "$STOCK_DUMP" > "$bootloader"
    if ! espflash flash --port "$PORT" --before no-reset --after no-reset \
        --flash-size 8mb --partition-table "$TABLE" \
        --partition-table-offset "$TABLE_OFFSET" --bootloader "$bootloader" \
        --erase-parts otadata \
        -B 921600 --non-interactive "$ELF"; then
        die "espflash failed.
     NOT running esptool — after a failed flash it leaves the box needing a
     J100 cold boot. Put the box back in download mode and try again."
    fi
fi

echo "flash: starting the firmware"
esptool --port "$PORT" --before no-reset --after watchdog-reset run >/dev/null

# esptool leaves the line in a state the console reader does not expect.
stty -F "$PORT" 115200 raw -echo
echo "flash: done"
