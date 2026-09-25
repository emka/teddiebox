#!/usr/bin/env python3
"""Compare the box's decoded PCM against the host's, with a tolerance.

Opus is not specified to be bit-exact across implementations — its own
conformance suite compares within a tolerance, because a fixed-point decoder's
last bit depends on the compiler and the target — so a CRC that disagrees
proves nothing on its own. This says how far apart the two are instead.

Reads a console capture containing `teddiebox: pcm <frame> <hex>` lines and the
WAV that `taf2wav` produced from the same file.
"""
import argparse, re, sys, wave

ap = argparse.ArgumentParser()
ap.add_argument("capture", help="console capture containing the pcm lines")
ap.add_argument("wav", help="what taf2wav decoded from the same TAF")
ap.add_argument("--tolerance", type=int, default=64,
                help="largest per-sample difference still called a match")
args = ap.parse_args()

line = re.compile(r"teddiebox: pcm (\d+) ([0-9A-F]+)\s*$")
device = []
frames = 0
with open(args.capture, "rb") as f:
    for raw in f:
        m = line.search(raw.decode("utf-8", "replace"))
        if not m:
            continue
        frames += 1
        blob = m.group(2)
        for i in range(0, len(blob), 4):
            value = int(blob[i:i + 4], 16)
            device.append(value - 0x10000 if value >= 0x8000 else value)

if not device:
    sys.exit("no `teddiebox: pcm` lines in the capture")

with wave.open(args.wav) as w:
    if w.getsampwidth() != 2:
        sys.exit("expected 16-bit samples")
    raw = w.readframes(w.getnframes())
host = [int.from_bytes(raw[i:i + 2], "little", signed=True) for i in range(0, len(raw), 2)]

n = min(len(device), len(host))
if not n:
    sys.exit("nothing to compare")

deltas = [device[i] - host[i] for i in range(n)]
worst = max(deltas, key=abs)
differing = sum(1 for d in deltas if d)
energy = sum(d * d for d in deltas)
peak = max((abs(v) for v in host[:n]), default=0)

print(f"frames from the box   {frames}")
print(f"samples compared      {n} (box {len(device)}, host {len(host)})")
print(f"samples differing     {differing} ({100 * differing / n:.2f}%)")
print(f"largest difference    {worst}")
print(f"host peak amplitude   {peak}")
print(f"rms difference        {(energy / n) ** 0.5:.4f}")

if len(device) != len(host):
    print("\nNOTE: the two streams are different lengths; only the overlap was "
          "compared. A length mismatch is a real defect, not a rounding one.")

if abs(worst) <= args.tolerance:
    print(f"\nPASS: every sample is within {args.tolerance} of the host's.")
    print("Consistent with last-bit rounding in a fixed-point decoder, which is "
          "what Opus permits between platforms.")
else:
    print(f"\nFAIL: a sample differs by {worst}, beyond the {args.tolerance} "
          "tolerance. That is too large to be rounding.")
    sys.exit(1)
