#!/usr/bin/env python3
"""Checks that the box reads the SD card correctly.

The box's `sd` command prints one checksum line per file. This prints the same
lines for the same card read on this machine, so the two can be compared with
`diff`.

    # with the card in a reader on this machine
    scripts/sd-checksums.py /run/media/user/TONIEBOX > host.txt

    # from a console capture taken while the box ran `sd`
    scripts/sd-checksums.py --from-capture bench.log > box.txt

    diff host.txt box.txt

Read-only on both sides.

Names are upper-cased and lines are sorted, because only the presentation
differs: FAT stores 8.3 names in upper case but Linux's vfat driver returns
some in lower case, and the box walks directories in on-disk order.

One difference remains: the box reads 8.3 short names, while this reads what
the vfat driver reports, which for a long-named file is the long name. A
Toniebox card only has `CONTENT/<8 hex>/<8 hex>` names, but on a card with
long names the diff would show a different path, not a different checksum.
"""

import argparse
import os
import re
import sys
import zlib

# `teddiebox: sd /CONTENT/00000000/500304E0 4198400 A1B2C3D4 (438 KiB/s)`
#
# The throughput is left out: it measures the bus, not the card, and would
# never match the host.
GOOD_LINE = re.compile(
    r"^teddiebox: sd (?P<path>/\S*) (?P<size>\d+) (?P<crc>[0-9A-F]{8})(?: \(|$)"
)

# Any other message the walk prints about a file. Kept, because a file that
# could not be read must not look like a match.
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
