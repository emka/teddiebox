#!/usr/bin/env python3
"""The host half of bench step 7.

Step 7 passes when a file on the SD card checksums the same read by the box as
read by a laptop. The box prints one line per file; this prints the same lines
for the same card, so the criterion is a `diff` rather than an eyeball.

    # with the card in a reader on this machine
    scripts/sd-checksums.py /run/media/user/TONIEBOX > host.txt

    # from a console capture taken while the box ran `sd`
    scripts/sd-checksums.py --from-capture bench.log > box.txt

    diff host.txt box.txt

Read-only on both sides: the card in the box is the one it shipped with, and
it is evidence.

Names are upper-cased and lines are sorted, because the two sides disagree
about neither the bytes nor the order but about presentation. FAT stores 8.3
names in upper case and Linux's vfat driver hands some of them back lowered,
and the box walks directories in on-disk order while a host walk does not.

One presentational difference this cannot paper over: the box reads 8.3 short
names from the directory entries, while this reads whatever the vfat driver
reports, which for a long-named file is the long name. A Toniebox card is
`CONTENT/<8 hex>/<8 hex>` throughout and has none, but on a card that did, the
diff would show a differing path rather than a differing checksum.
"""

import argparse
import os
import re
import sys
import zlib

# `teddiebox: sd /CONTENT/00000000/500304E0 4198400 A1B2C3D4 (438 KiB/s)`
#
# The throughput is deliberately outside the capture: it is a measurement of
# this bus on this day, not a property of the card, and comparing it against a
# laptop would be meaningless.
GOOD_LINE = re.compile(
    r"^teddiebox: sd (?P<path>/\S*) (?P<size>\d+) (?P<crc>[0-9A-F]{8})(?: \(|$)"
)

# Anything else the walk says about a particular file. These are reported, not
# dropped: a card whose files went unread is not a card that matched.
BAD_LINE = re.compile(r"^teddiebox: sd (?P<path>/\S*) (?P<why>UNREADABLE|SHORT|READ|SKIPPED)\b")


def from_card(mountpoint):
    """Every file under `mountpoint`, as the box would name it."""
    lines = []
    for directory, _subdirs, files in os.walk(mountpoint):
        for name in files:
            full = os.path.join(directory, name)
            if os.path.islink(full) or not os.path.isfile(full):
                continue
            relative = os.path.relpath(full, mountpoint)
            path = "/" + relative.replace(os.sep, "/").upper()
            crc = 0
            size = 0
            with open(full, "rb") as handle:
                while True:
                    chunk = handle.read(1 << 16)
                    if not chunk:
                        break
                    crc = zlib.crc32(chunk, crc)
                    size += len(chunk)
            lines.append(f"{path} {size} {crc:08X}")
    return lines


def from_capture(path):
    """The same lines, recovered from a console log."""
    lines = []
    complaints = []
    with open(path, "r", errors="replace") as handle:
        for raw in handle:
            raw = raw.strip()
            good = GOOD_LINE.match(raw)
            if good:
                lines.append(
                    f"{good['path']} {good['size']} {good['crc']}"
                )
                continue
            bad = BAD_LINE.match(raw)
            if bad:
                complaints.append(raw)

    for complaint in complaints:
        print(f"note: the box could not read a file: {complaint}", file=sys.stderr)
    if not lines:
        print(
            "note: no checksum lines found — was the capture running when `sd` ran?",
            file=sys.stderr,
        )
    return lines


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "source",
        help="a mounted card, or a console capture with --from-capture",
    )
    parser.add_argument(
        "--from-capture",
        action="store_true",
        help="read the box's own output instead of walking a mounted card",
    )
    args = parser.parse_args()

    lines = from_capture(args.source) if args.from_capture else from_card(args.source)
    for line in sorted(lines):
        print(line)


if __name__ == "__main__":
    main()
