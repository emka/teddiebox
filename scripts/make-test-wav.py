#!/usr/bin/env python3
"""Generate a test WAV for the box's `wav` command to play from the SD card.

Used to test several minutes of uninterrupted playback. A Toniebox card has no
plain WAV files, so this makes one in the format the codec is configured for:
48 kHz, stereo, 16-bit PCM.

    scripts/make-test-wav.py TEST.WAV

The signal is chosen so a dropout is obvious rather than subtle:

- A steady tone, so any gap, click or repeat is audible against it.
- The pitch steps every 15 seconds, so you can hear roughly how far in you are
  without watching the console, and a repeated segment gives itself away.
- A brief silence before each step, which is where a stalled buffer will most
  obviously stutter.
- Quiet by default, because the `wav` command plays at a fixed volume and
  the speaker amplifier adds gain.
"""

import argparse
import math
import struct
import sys
import zlib

SAMPLE_RATE = 48_000
CHANNELS = 2
BITS = 16

# A tenth of full scale: loud enough to hear a gap, quiet enough to listen to
# for five minutes.
AMPLITUDE = 3200

# Pitches chosen to be easy to tell apart by ear, not to be musical.
TONES_HZ = [440, 550, 660, 880, 1100]
SEGMENT_SECONDS = 15
SILENCE_MS = 120


def samples(duration_s):
    """Interleaved stereo frames for the whole file."""
    total = 0
    segment = 0
    silence = int(SAMPLE_RATE * SILENCE_MS / 1000)
    want = int(SAMPLE_RATE * duration_s)

    while total < want:
        hz = TONES_HZ[segment % len(TONES_HZ)]
        segment += 1
        length = min(SAMPLE_RATE * SEGMENT_SECONDS, want - total)

        for n in range(length):
            if n < silence:
                value = 0
            else:
                # Phase from the position within this segment, so each segment
                # starts at a zero crossing and the joins do not click.
                value = int(AMPLITUDE * math.sin(2 * math.pi * hz * (n - silence) / SAMPLE_RATE))
            yield value
        total += length


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output")
    parser.add_argument("--minutes", type=float, default=5.0)
    args = parser.parse_args()

    duration = args.minutes * 60
    frames = bytearray()
    for value in samples(duration):
        frame = struct.pack("<hh", value, value)
        frames += frame

    data_len = len(frames)
    byte_rate = SAMPLE_RATE * CHANNELS * BITS // 8
    block_align = CHANNELS * BITS // 8

    header = b"RIFF" + struct.pack("<I", 36 + data_len) + b"WAVE"
    header += b"fmt " + struct.pack(
        "<IHHIIHH", 16, 1, CHANNELS, SAMPLE_RATE, byte_rate, block_align, BITS
    )
    header += b"data" + struct.pack("<I", data_len)

    with open(args.output, "wb") as out:
        out.write(header)
        out.write(frames)

    crc = zlib.crc32(header)
    crc = zlib.crc32(frames, crc)
    print(f"{args.output}: {len(header) + data_len} bytes, {duration:.0f} s, CRC32 {crc:08X}")
    print("Copy it to the SD card; the box plays the first .WAV it finds.")
    print("Step 7's checksum walk will confirm it landed byte for byte.")


if __name__ == "__main__":
    main()
