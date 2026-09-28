#!/usr/bin/env python3
"""Talk to the box on UART0: optionally send a line, then read until a marker.

Reads with a deadline rather than a fixed wait, so a walk that takes minutes
and one that takes seconds both work, and the port is released the moment the
marker arrives. Releasing it matters: a reader left holding /dev/ttyUSB0 blocks
every flashing tool afterwards.
"""
import argparse
import os
import re
import select
import sys
import termios
import time
import tty


def configure(fd):
    """Sets the port to what the box's console speaks: 115200, raw, no echo.

    Set here rather than left to an earlier `stty`: an adapter that is
    plugged in again comes back at 9600, and at that speed even `dl` is
    never understood, which looks exactly like a box that stopped listening.
    """
    tty.setraw(fd)
    attrs = termios.tcgetattr(fd)
    attrs[4] = attrs[5] = termios.B115200
    termios.tcsetattr(fd, termios.TCSANOW, attrs)


def self_test():
    """Configures a pseudo-terminal left at 9600 and cooked, as a freshly
    plugged adapter comes up, and checks it ends at 115200 and raw."""
    parent, child = os.openpty()
    try:
        attrs = termios.tcgetattr(child)
        attrs[4] = attrs[5] = termios.B9600
        attrs[3] |= termios.ICANON | termios.ECHO
        termios.tcsetattr(child, termios.TCSANOW, attrs)

        configure(child)

        _iflag, _oflag, _cflag, lflag, ispeed, ospeed, _cc = termios.tcgetattr(child)
        assert ispeed == termios.B115200, ispeed
        assert ospeed == termios.B115200, ospeed
        assert not lflag & termios.ICANON, "still line-buffered"
        assert not lflag & termios.ECHO, "still echoing"
    finally:
        os.close(parent)
        os.close(child)
    print("self-test ok")


ap = argparse.ArgumentParser()
ap.add_argument("--port", default="/dev/ttyUSB0")
ap.add_argument("--send", help="line to write once the reader is listening")
ap.add_argument("--send-after", type=float, default=0.5, help="seconds to listen before sending")
ap.add_argument("--until", help="regex that ends the capture as soon as it matches")
ap.add_argument("--timeout", type=float, default=10.0)
ap.add_argument("--out")
ap.add_argument("--self-test", action="store_true")
args = ap.parse_args()
if args.self_test:
    self_test()
    sys.exit(0)
if not args.out:
    ap.error("--out is required")

marker = re.compile(args.until) if args.until else None
fd = os.open(args.port, os.O_RDWR | os.O_NOCTTY | os.O_NONBLOCK)
configure(fd)
deadline = time.time() + args.timeout
send_at = time.time() + args.send_after if args.send else None
pending = (args.send + "\r").encode() if args.send else None
buf = b""
matched = False

try:
    with open(args.out, "wb") as out:
        while time.time() < deadline:
            if pending is not None and time.time() >= send_at:
                os.write(fd, pending)
                print(f"--- sent {args.send!r}", file=sys.stderr)
                pending = None
            ready, _, _ = select.select([fd], [], [], 0.2)
            if not ready:
                continue
            try:
                chunk = os.read(fd, 4096)
            except BlockingIOError:
                continue
            if not chunk:
                continue
            out.write(chunk)
            out.flush()
            sys.stdout.write(chunk.decode("utf-8", "replace"))
            sys.stdout.flush()
            if marker:
                buf += chunk
                if marker.search(buf.decode("utf-8", "replace")):
                    matched = True
                    break
finally:
    os.close(fd)

if marker and not matched:
    print(f"\n--- marker {args.until!r} never appeared within {args.timeout}s", file=sys.stderr)
    sys.exit(1)
