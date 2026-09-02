#!/usr/bin/env python3
"""Talk to the box on UART0: optionally send a line, then read until a marker.

Reads with a deadline rather than a fixed wait, so a walk that takes minutes
and one that takes seconds both work, and the port is released the moment the
marker arrives. Releasing it matters: a reader left holding /dev/ttyUSB0 blocks
every flashing tool afterwards.
"""
import argparse, os, re, select, sys, time

ap = argparse.ArgumentParser()
ap.add_argument("--port", default="/dev/ttyUSB0")
ap.add_argument("--send", help="line to write once the reader is listening")
ap.add_argument("--send-after", type=float, default=0.5, help="seconds to listen before sending")
ap.add_argument("--until", help="regex that ends the capture as soon as it matches")
ap.add_argument("--timeout", type=float, default=10.0)
ap.add_argument("--out", required=True)
args = ap.parse_args()

marker = re.compile(args.until) if args.until else None
fd = os.open(args.port, os.O_RDWR | os.O_NOCTTY | os.O_NONBLOCK)
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
